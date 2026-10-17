//! Who installs a verified set: the [`Applier`] interface, a [`FakeApplier`] for tests,
//! and [`AptSnapshot`], the Linux pinned-package applier of section 8.1 of
//! `docs/design/fleet-updates.md`.
//!
//! The updater calls an applier only with a set every check of S3.1 passed and whose
//! changed artifacts are already staged and hashed. An applier must be idempotent: after
//! a crash mid-apply the updater calls it again with the same set.
//!
//! [`AptSnapshot`] changes the node only through `kbf-host`, the host-change path it
//! shares with `kbf-deploy`: it builds a `kbf_host::Plan` and applies it with
//! `kbf_host::execute` as [`RunAs::Root`], so the plan is refused unless the updater
//! runs as root from a binary only root can replace, and each step is logged as
//! `+ <step>` (to stderr on a node) before it runs. The binary gives it
//! `kbf_host::HostExec` on `/`; the tests give it `kbf_host::testkit::FakeExec`, which
//! only records, so no test changes a package on any machine. What the plan holds:
//!
//! 1. with a snapshot pinned: `/usr/bin/apt-get --snapshot <id> update`, then
//!    `... --yes full-upgrade`, then `... --yes install <kernel>` (apt checks every
//!    package against the archive's signature; the set chooses only which snapshot).
//!    Each runs with only `DEBIAN_FRONTEND=noninteractive` (apt never waits for a
//!    terminal) and `PATH` (dpkg refuses to run without one) in its environment;
//! 2. every staged artifact, as a file in the bin directory with mode 0755 owned by
//!    root (uid and gid 0), written beside the installed one, synced and renamed over
//!    it, so a crash leaves the old binary or the new. An installed file that already
//!    holds the staged bytes with that mode and owner is left alone, so a resumed apply
//!    rewrites nothing already in place; one with the right bytes that another user
//!    owns is replaced.
//!
//! The step needs a reboot exactly when `/run/reboot-required` exists afterwards, and
//! the reboot is a one-step plan, `/usr/bin/systemctl reboot`.
//!
//! **Not done yet: restarting what it replaced.** Nothing restarts a service after its binary
//! is renamed over; the pull-based helper (`kbf-helper`) does that with versioned directories.

use std::fmt::Display;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use kbf_host::{Cmd, Exec, HostPath, Mode, Owner, Perms, Plan, Planner, RunAs, execute};

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

/// apt, by absolute path: the environment it runs with names no `PATH` to search.
pub const APT_GET: &str = "/usr/bin/apt-get";
/// systemd's control tool, which starts the reboot.
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";
/// The file whose presence means the node needs a reboot.
pub const REBOOT_REQUIRED: &str = "/run/reboot-required";
/// The `PATH` apt runs with: dpkg refuses to run without one, and its maintainer scripts
/// find their tools through it.
pub const APT_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
/// The mode of an installed binary.
const BINARY: Perms = Perms::new(0o755);
/// Who owns an installed binary: root, user and group. Root alone may replace what root
/// runs, and the trust check refuses an updater binary anyone else owns.
const ROOT: Owner = Owner { uid: 0, gid: 0 };

/// One of the fixed absolute paths above as a [`HostPath`].
fn fixed(path: &'static str) -> HostPath {
    HostPath::new(path).expect("a fixed path is absolute and plain")
}

/// The message of an install or reboot error.
fn failed(e: impl Display) -> String {
    e.to_string()
}

/// The pinned-package applier for Linux (section 8.1).
#[derive(Clone, Debug)]
pub struct AptSnapshot<E, W = io::Stderr> {
    /// The host the plans change: `kbf_host::HostExec` on `/`, or a fake in tests.
    pub exec: E,
    /// Where kbf's binaries are installed, one file per artifact name.
    pub bin_dir: HostPath,
    /// Where each applied step is logged as `+ <step>`: stderr, so the journal, on a node.
    pub log: W,
}

impl<E: Exec> AptSnapshot<E> {
    /// The applier a node uses: kbf binaries in `bin_dir`, changed through `exec`.
    pub fn new(exec: E, bin_dir: HostPath) -> Self {
        AptSnapshot {
            exec,
            bin_dir,
            log: io::stderr(),
        }
    }
}

impl<E: Exec, W: Write> AptSnapshot<E, W> {
    /// The plan that installs `set` from `staged` (items 1 and 2 of the module docs).
    ///
    /// # Errors
    /// A staged artifact cannot be read, or the host cannot say what is installed.
    fn plan(&self, set: &SoftwareSet, staged: &Path) -> Result<Plan, String> {
        let mut planner = Planner::new(&self.exec);
        if let Some(pin) = &set.snapshot {
            let steps: [&[&str]; 3] = [
                &["update"],
                &["--yes", "full-upgrade"],
                &["--yes", "install", pin.kernel.as_str()],
            ];
            for args in steps {
                let cmd = Cmd::new(fixed(APT_GET))
                    .arg("--snapshot")
                    .arg(pin.snapshot.as_str());
                let cmd = args.iter().fold(cmd, |cmd, a| cmd.arg(*a));
                planner.run(
                    cmd.env("DEBIAN_FRONTEND", "noninteractive")
                        .env("PATH", APT_PATH),
                );
            }
        }
        for name in set.artifacts.keys() {
            let bytes = match fs::read(staged.join(name)) {
                Ok(bytes) => bytes,
                // Not staged: the set does not change it.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("install {name}: {e}")),
            };
            let path = HostPath::new(&format!("{}/{name}", self.bin_dir)).map_err(failed)?;
            planner
                .file(path, BINARY, Some(ROOT), bytes)
                .map_err(failed)?;
        }
        Ok(planner.finish())
    }

    /// Applies `plan` as root, logging each step to `log`.
    fn carry_out(&mut self, plan: &Plan) -> Result<(), String> {
        execute(
            plan,
            RunAs::Root,
            Mode::Apply,
            &mut self.exec,
            &mut self.log,
        )
        .map_err(failed)
    }
}

impl<E: Exec, W: Write> Applier for AptSnapshot<E, W> {
    fn install(&mut self, set: &SoftwareSet, staged: &Path) -> Result<Installed, String> {
        let plan = self.plan(set, staged)?;
        self.carry_out(&plan)?;
        let flag = self.exec.inspect(&fixed(REBOOT_REQUIRED)).map_err(failed)?;
        Ok(Installed {
            reboot: flag.is_some(),
        })
    }

    fn reboot(&mut self) -> Result<(), String> {
        let mut planner = Planner::new(&self.exec);
        planner.run(Cmd::new(fixed(SYSTEMCTL)).arg("reboot"));
        let plan = planner.finish();
        self.carry_out(&plan)
    }
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod tests;
