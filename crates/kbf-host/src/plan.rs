//! Building a plan from what the host has, and printing or applying it.

use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::exec::{Entry, Exec, Kind, Meta};
use crate::path::HostPath;
use crate::step::{Cmd, Filter, HttpUrl, Owner, Perms, Step};
use crate::trust::Untrusted;

/// The steps that bring the host to what a kit wants, in order.
///
/// Only a [`Planner`] makes one, so every file, directory and link step in it is a change
/// the host does not have yet, and a plan made again after it was applied has none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    steps: Vec<Step>,
}

impl Plan {
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// Builds a [`Plan`], asking the host through `exec` what is already there.
pub struct Planner<'a, E: Exec> {
    exec: &'a E,
    plan: Plan,
}

impl<'a, E: Exec> Planner<'a, E> {
    pub fn new(exec: &'a E) -> Planner<'a, E> {
        Planner {
            exec,
            plan: Plan::default(),
        }
    }

    /// The file at `path` holds `bytes` with `perms` (and `owner`, when given). Returns
    /// whether a step was planned, so a kit can plan what follows a change (a
    /// `systemctl daemon-reload`) only when there is one.
    pub fn file(
        &mut self,
        path: HostPath,
        perms: Perms,
        owner: Option<Owner>,
        bytes: Vec<u8>,
    ) -> Result<bool, HostError> {
        let done = match self.inspect(&path)? {
            Some(Entry::File {
                perms: p,
                owner: o,
                bytes: b,
            }) => p == perms && owner.is_none_or(|w| w == o) && b == bytes,
            _ => false,
        };
        if !done {
            self.plan.steps.push(Step::WriteFile {
                path,
                perms,
                owner,
                bytes,
            });
        }
        Ok(!done)
    }

    /// A directory is at `path` with `perms` (and `owner`, when given). Returns whether a
    /// step was planned.
    pub fn dir(
        &mut self,
        path: HostPath,
        perms: Perms,
        owner: Option<Owner>,
    ) -> Result<bool, HostError> {
        let done = match self.stat(&path)? {
            Some(Meta {
                kind: Kind::Dir,
                perms: p,
                owner: o,
                ..
            }) => p == perms && owner.is_none_or(|w| w == o),
            _ => false,
        };
        if !done {
            self.plan.steps.push(Step::Mkdir { path, perms, owner });
        }
        Ok(!done)
    }

    /// Nothing is at `path`; what is there is never read (it may be a swapfile).
    /// Returns whether a step was planned.
    pub fn absent(&mut self, path: HostPath) -> Result<bool, HostError> {
        let there = self.stat(&path)?.is_some();
        if there {
            self.plan.steps.push(Step::Remove { path });
        }
        Ok(there)
    }

    /// The symbolic link at `path` points to `target`; when it points elsewhere or is
    /// missing, a [`Step::Symlink`] replaces or makes it. Returns whether a step was
    /// planned. A file, directory or anything else at `path` is refused
    /// ([`HostError::NotALink`]): the step would never replace it.
    pub fn symlink(&mut self, path: HostPath, target: PathBuf) -> Result<bool, HostError> {
        let done = match self.inspect(&path)? {
            Some(Entry::Link { target: t }) => t == target,
            None => false,
            Some(_) => return Err(HostError::NotALink { path }),
        };
        if !done {
            self.plan.steps.push(Step::Symlink { path, target });
        }
        Ok(!done)
    }

    /// Run `cmd`. The host cannot say whether a command is needed, so the kit asks
    /// first and calls this only when it is.
    pub fn run(&mut self, cmd: Cmd) {
        self.plan.steps.push(Step::Run(cmd));
    }

    /// Run `cmd` and write what `filter` makes of its output to `path`. Like [`run`],
    /// the kit asks first (is a file at `path` already?) and calls this only when it
    /// is needed.
    ///
    /// [`run`]: Planner::run
    pub fn capture(
        &mut self,
        cmd: Cmd,
        filter: Filter,
        path: HostPath,
        perms: Perms,
        owner: Option<Owner>,
    ) {
        self.plan.steps.push(Step::Capture {
            cmd,
            filter,
            path,
            perms,
            owner,
        });
    }

    /// Wait for `url` to answer, for at most `deadline`.
    pub fn wait_http(&mut self, url: HttpUrl, deadline: Duration) {
        self.plan.steps.push(Step::WaitHttp { url, deadline });
    }

    /// The steps planned so far.
    pub fn steps(&self) -> &[Step] {
        &self.plan.steps
    }

    pub fn finish(self) -> Plan {
        self.plan
    }

    fn inspect(&self, path: &HostPath) -> Result<Option<Entry>, HostError> {
        self.exec
            .inspect(path)
            .map_err(|source| HostError::Inspect {
                path: path.clone(),
                source,
            })
    }

    fn stat(&self, path: &HostPath) -> Result<Option<Meta>, HostError> {
        self.exec.stat(path).map_err(|source| HostError::Inspect {
            path: path.clone(),
            source,
        })
    }
}

/// Print the plan, or carry it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Print each step as `DRY-RUN: <step>`; change nothing. The default.
    DryRun,
    /// Print each step as `+ <step>`, then do it (`--apply`).
    Apply,
}

/// Who may apply a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunAs {
    /// Only root (effective uid 0), from a binary only root can replace.
    Root,
    /// Only a user other than root.
    User,
}

/// Why a plan could not be made or carried out.
#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("this step changes the host as root: run it with sudo (effective uid is {euid})")]
    NotRoot { euid: u32 },
    #[error("this step runs as a user, not as root: run it without sudo")]
    IsRoot,
    #[error("refusing to run as root from a binary root does not own alone: {0}")]
    Untrusted(#[from] Untrusted),
    #[error("refusing to replace {path} with a symbolic link: it is not one")]
    NotALink { path: HostPath },
    #[error("cannot read {path}: {source}")]
    Inspect { path: HostPath, source: io::Error },
    #[error("step {index} ({step}) failed: {source}")]
    Step {
        index: usize,
        step: String,
        source: io::Error,
    },
    #[error("cannot write the plan out: {0}")]
    Output(io::Error),
}

/// The last line of a dry run that would change something.
pub const DRY_RUN_NOTE: &str = "dry run: nothing changed. Re-run with --apply.";
/// The only line of a plan with no steps.
pub const NOTHING_TO_DO: &str = "nothing to do";

/// Print `plan` to `out`, and in [`Mode::Apply`] carry it out through `exec`, stopping
/// at the first step that fails.
///
/// Applying checks `run_as` against the effective uid first; as root it also checks
/// that the running binary and every directory above it are owned by root and not
/// writable by group or others. A dry run checks neither: it changes nothing.
pub fn execute<E: Exec, W: Write>(
    plan: &Plan,
    run_as: RunAs,
    mode: Mode,
    exec: &mut E,
    out: &mut W,
) -> Result<(), HostError> {
    if mode == Mode::Apply {
        let euid = exec.euid();
        match run_as {
            RunAs::Root if euid != 0 => return Err(HostError::NotRoot { euid }),
            RunAs::User if euid == 0 => return Err(HostError::IsRoot),
            _ => {}
        }
        if euid == 0 {
            exec.check_binary()?;
        }
    }
    if plan.is_empty() {
        return writeln!(out, "{NOTHING_TO_DO}").map_err(HostError::Output);
    }
    for (index, step) in plan.steps().iter().enumerate() {
        match mode {
            Mode::DryRun => writeln!(out, "DRY-RUN: {step}").map_err(HostError::Output)?,
            Mode::Apply => {
                writeln!(out, "+ {step}").map_err(HostError::Output)?;
                exec.apply(step).map_err(|source| HostError::Step {
                    index,
                    step: step.to_string(),
                    source,
                })?;
            }
        }
    }
    if mode == Mode::DryRun {
        writeln!(out, "{DRY_RUN_NOTE}").map_err(HostError::Output)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
