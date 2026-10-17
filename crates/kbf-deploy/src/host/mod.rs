//! The host side of commit-confirm deploys: where a host keeps kbf's releases and
//! kbf-deploy's own state, and the `host` verbs that read and change them.
//!
//! - [`layout`]: the paths: each release in its own directory under
//!   `/usr/local/kbf/releases/<commit>/`, the `current` and `previous` links beside them,
//!   kbf-deploy's state under `/var/lib/kbf-deploy/` and its settings under
//!   `/etc/kbf-deploy/`.
//! - [`state`]: `state.json` (the current, previous and staged commits, the one waiting
//!   to be confirmed, and the order floor), read and written under an exclusive lock on
//!   `/run/kbf-deploy.lock` and replaced whole by a rename.
//! - [`release`]: the manual `install`, `rollback` and `gc` verbs.
//! - [`CommitId`]: a commit as 40 lowercase hex characters, the only form a release is
//!   named by.
//! - [`state_store`]: the SQLite state store's file backed up while the frontend runs
//!   and restored while it is stopped.
//!
//! The verbs (`kbf-deploy host ...`):
//!
//! ```text
//! kbf-deploy host status [--json] [--root DIR]
//! kbf-deploy host install --commit <40 hex> --from <dir> [--apply] [--root DIR]
//! kbf-deploy host rollback [--apply] [--root DIR]
//! kbf-deploy host gc [--keep N] [--apply] [--root DIR]
//! kbf-deploy host state-backup --db FILE --dest FILE [--owner USER[:GROUP]] [--mode 0640]
//! kbf-deploy host state-restore --src FILE --db FILE [--apply]
//! ```
//!
//! What `state-backup` and `state-restore` do and exit is in [`state_store`].
//!
//! `status` only reads: it prints the state and whether the `current` link points at
//! the state's current release, and exits 0 when it does (or when there is neither), 1
//! when it does not, or 2 refused (`REFUSED: ...` on stderr: a state file that cannot
//! be read or parsed). `--root` names the directory the host's paths are under (`/` by
//! default), such as a mounted image.
//!
//! The state file is root's alone (0600), so `status` and the dry runs read it with
//! sudo too. `install`, `rollback` and `gc` print their plan and change nothing unless
//! `--apply` is given; what each does is in [`release`]. They exit 0 when done and 1
//! with `kbf-deploy: <why>` on stderr when they refuse or a step fails.

pub mod layout;
pub mod release;
pub mod state;
pub mod state_store;

use std::fmt;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand};
use kbf_host::exec::HostExec;
use kbf_host::path::Root;
use kbf_host::plan::Mode;
use serde::{Deserialize, Serialize};

use state::{CurrentLink, Status};

/// A commit, as Git names it in full: exactly 40 lowercase hex characters. A release
/// directory, a link target and the state name commits only in this form, so a short or
/// upper-case id can never name a second copy of one release.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitId(String);

/// Why a string is not a [`CommitId`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a commit id (40 lowercase hex characters)")]
pub struct NotACommit(pub String);

impl CommitId {
    pub fn new(id: &str) -> Result<CommitId, NotACommit> {
        let hex = |b: u8| matches!(b, b'0'..=b'9' | b'a'..=b'f');
        if id.len() == 40 && id.bytes().all(hex) {
            Ok(CommitId(id.to_owned()))
        } else {
            Err(NotACommit(id.to_owned()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CommitId {
    type Error = NotACommit;

    fn try_from(id: String) -> Result<CommitId, NotACommit> {
        CommitId::new(&id)
    }
}

impl From<CommitId> for String {
    fn from(id: CommitId) -> String {
        id.0
    }
}

impl std::str::FromStr for CommitId {
    type Err = NotACommit;

    fn from_str(id: &str) -> Result<CommitId, NotACommit> {
        CommitId::new(id)
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `host` verbs of the `kbf-deploy` command line ([`crate::cli`]).
#[derive(Subcommand)]
pub enum Host {
    /// Print the deploy state and whether the current link matches it. Only reads.
    Status(StatusArgs),
    /// Copy a release into its directory and make it current. Prints the plan unless
    /// --apply.
    Install(InstallArgs),
    /// Make the previous release current again. Prints the plan unless --apply.
    Rollback(ApplyArgs),
    /// Remove old release directories, never the current, previous or staged one.
    /// Prints the plan unless --apply.
    Gc(GcArgs),
    /// Copy the SQLite state store's file while the frontend runs. Writes only --dest.
    StateBackup(state_store::BackupArgs),
    /// Put a state-backup copy back in place of the state file, while the frontend is
    /// stopped. Prints the plan unless --apply.
    StateRestore(state_store::RestoreArgs),
}

#[derive(Args)]
pub struct StatusArgs {
    /// Print one JSON object instead of lines.
    #[arg(long)]
    json: bool,
    /// The directory the host's paths are under, such as a mounted image.
    #[arg(long, default_value = "/", value_parser = root)]
    root: Root,
}

#[derive(Args)]
pub struct InstallArgs {
    /// The release's commit: 40 lowercase hex characters.
    #[arg(long)]
    commit: CommitId,
    /// The directory holding the release: `kbf`, and `units/` when it has units.
    #[arg(long)]
    from: PathBuf,
    #[command(flatten)]
    apply: ApplyArgs,
}

#[derive(Args)]
pub struct GcArgs {
    /// Keep this many release directories besides the current, previous and staged ones,
    /// the newest first.
    #[arg(long, default_value_t = 0)]
    keep: usize,
    #[command(flatten)]
    apply: ApplyArgs,
}

#[derive(Args)]
pub struct ApplyArgs {
    /// Carry the plan out (as root).
    #[arg(long)]
    apply: bool,
    /// The directory the host's paths are under, such as a mounted image.
    #[arg(long, default_value = "/", value_parser = root)]
    root: Root,
}

impl ApplyArgs {
    fn mode(&self) -> Mode {
        if self.apply {
            Mode::Apply
        } else {
            Mode::DryRun
        }
    }
}

fn root(dir: &str) -> Result<Root, kbf_host::PathError> {
    Root::new(PathBuf::from(dir))
}

/// Run a `host` verb: see the module docs for what each prints and its exit.
pub fn run(verb: Host, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let done = match verb {
        Host::Status(a) => return status(&a.root, a.json, out, err),
        Host::StateBackup(a) => return state_store::run_backup(a, out, err),
        Host::StateRestore(a) => return state_store::run_restore(a, out, err),
        Host::Install(a) => {
            let r = &a.apply.root;
            let what = release::Install {
                commit: a.commit,
                from: a.from,
            };
            let mode = a.apply.mode();
            release::install(
                &mut HostExec::new(r.clone()),
                r,
                release::ROOT,
                &what,
                mode,
                out,
            )
        }
        Host::Rollback(a) => {
            release::rollback(&mut HostExec::new(a.root.clone()), &a.root, a.mode(), out)
        }
        Host::Gc(a) => {
            let r = &a.apply.root;
            release::gc(
                &mut HostExec::new(r.clone()),
                r,
                a.keep,
                a.apply.mode(),
                out,
            )
        }
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "kbf-deploy: {e}");
            ExitCode::FAILURE
        }
    }
}

fn status(root: &Root, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let status = match state::status(root) {
        Ok(status) => status,
        Err(e) => {
            let _ = writeln!(err, "REFUSED: {e}");
            return ExitCode::from(2);
        }
    };
    let text = if json {
        format!("{}\n", status_json(&status))
    } else {
        status_lines(&status)
    };
    let _ = out.write_all(text.as_bytes());
    ExitCode::from(u8::from(!status.link_matches()))
}

/// `none`, or the commit.
struct Or<'a>(&'a Option<CommitId>);

impl fmt::Display for Or<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(c) => write!(f, "{c}"),
            None => f.write_str("none"),
        }
    }
}

/// The lines `host status` prints.
pub fn status_lines(status: &Status) -> String {
    let s = &status.state;
    let mut text = format!("state: {}", layout::STATE_FILE);
    if !status.exists {
        text.push_str(" (none yet)");
    }
    text.push_str(&format!(
        "\ncurrent: {}\nprevious: {}\nstaged: {}\n",
        Or(&s.current),
        Or(&s.previous),
        Or(&s.staged)
    ));
    match &s.pending_confirm {
        Some(p) => text.push_str(&format!(
            "pending confirm: {} by {} (unix seconds), else back to {}\n",
            p.commit,
            p.deadline,
            Or(&p.previous)
        )),
        None => text.push_str("pending confirm: none\n"),
    }
    text.push_str(&format!("order floor: {}\n", s.order_floor));
    let link = match &status.link {
        CurrentLink::Missing => "missing".to_owned(),
        CurrentLink::To(target) => target.display().to_string(),
        CurrentLink::NotALink => "not a symbolic link".to_owned(),
    };
    let verdict = if status.link_matches() {
        "matches the state".to_owned()
    } else {
        match &s.current {
            Some(c) => format!("the state says {}", layout::release_link(c).display()),
            None => "the state has no current release".to_owned(),
        }
    };
    text.push_str(&format!("current link: {link} ({verdict})\n"));
    text
}

/// The object `host status --json` prints.
pub fn status_json(status: &Status) -> serde_json::Value {
    let (kind, target) = match &status.link {
        CurrentLink::Missing => ("missing", None),
        CurrentLink::To(target) => ("link", Some(target.display().to_string())),
        CurrentLink::NotALink => ("not_a_link", None),
    };
    serde_json::json!({
        "schema": state::SCHEMA,
        "state_file": layout::STATE_FILE,
        "exists": status.exists,
        "state": status.state,
        "current_link": {
            "kind": kind,
            "target": target,
            "matches_state": status.link_matches(),
        },
    })
}

#[cfg(test)]
mod tests;
