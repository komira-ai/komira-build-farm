//! `crun`, driven through the `runc` crate (containerd/rust-extensions, 0.3, the async
//! client): the OCI runtime command line `crun run --bundle`, `kill` and `delete`.
//!
//! What the crate does and does not give this driver, checked in its source (0.3.0):
//! - `GlobalOpts::command(path)` sets the binary every call runs, so `crun` is used as
//!   it is for `runc` (`crun` takes the same global options `--root`, `--log` and
//!   `--log-format`, and the same `run`/`kill`/`delete` verbs). `--rootless` is left
//!   unset: `crun` finds out itself, and has no such option.
//! - It has no `--cgroup-manager` option; `crun`'s default is `cgroupfs`, which the
//!   daemon's delegated subtree needs.
//! - `Runc::run` waits for the process and returns `Error::CommandFailed { status, .. }`
//!   for a non-zero exit, so the container's exit status is `crun run`'s own: no
//!   `state` query and no window in which the container is stopped but its exit code is
//!   not recorded (kbf#658).
//! - It runs its commands with `tokio::process`, without `kill_on_drop`; [`KillOnDrop`]
//!   is the spawner that adds it, so a `run` that is dropped or aborted does not leave
//!   `crun` behind.
//! - Its `Io` hook sets the child's stdin/stdout/stderr; [`FileIo`] points the
//!   container's stdout and stderr at the lease's files instead of the crate's pipes
//!   (an action's output is not held in memory).
//! - The synchronous client is the same crate without its `async` feature, so the two
//!   cannot be built together: the blocking paths (a dropped lease's clean) use
//!   `std::process` with the same arguments ([`Crun::delete_blocking`]).

use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::Arc;

use async_trait::async_trait;
use runc::error::Error;
use runc::options::{CreateOpts, GlobalOpts};
use runc::{Command, Io, Runc, Spawner};

use crate::credential::without_credentials;

/// How one `crun run` ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RunEnd {
    /// `crun run` exited with this status: the container's exit code (`128 + n` if it
    /// was killed by signal `n`), or `crun`'s own failure, told apart by the log.
    Exited(i32),
    /// `crun` could not be run, or its task did not finish.
    Failed(String),
}

/// The exit status a shell would report for `status`.
pub(crate) fn code_of(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// The `Spawner` of this driver: [`runc::DefaultExecutor`], with `kill_on_drop`.
#[derive(Debug)]
struct KillOnDrop;

#[async_trait]
impl Spawner for KillOnDrop {
    async fn execute(&self, mut cmd: Command) -> runc::Result<(ExitStatus, u32, String, String)> {
        cmd.kill_on_drop(true);
        without_credentials(cmd.as_std_mut());
        let child = cmd.spawn().map_err(Error::ProcessSpawnFailed)?;
        let pid = child.id().unwrap_or(0);
        let out = child
            .wait_with_output()
            .await
            .map_err(Error::InvalidCommand)?;
        Ok((
            out.status,
            pid,
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}

/// `Io` that gives `crun run` the lease's stdout and stderr files, and no stdin.
#[derive(Debug)]
struct FileIo {
    stdout: std::fs::File,
    stderr: std::fs::File,
}

#[async_trait]
impl Io for FileIo {
    async fn set(&self, cmd: &mut Command) -> std::io::Result<()> {
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(self.stdout.try_clone()?);
        cmd.stderr(self.stderr.try_clone()?);
        Ok(())
    }

    async fn close_after_start(&self) {}
}

/// The `crun` program and how the driver calls it.
#[derive(Clone, Debug)]
pub(crate) struct Crun {
    program: PathBuf,
}

/// The state directory of a lease's `crun` (`--root`), inside the lease directory: no
/// state is shared between actions, and removing the lease removes it.
pub(crate) fn state_dir(lease: &Path) -> PathBuf {
    lease.join("crun")
}

/// `crun`'s log of a lease (`--log`, JSON).
pub(crate) fn log_path(lease: &Path) -> PathBuf {
    lease.join("crun.log")
}

impl Crun {
    pub(crate) fn new(program: PathBuf) -> Self {
        Self { program }
    }

    /// The client for the lease in `lease`: its own state directory and log.
    fn client(&self, lease: &Path) -> Result<Runc, String> {
        let mut opts = GlobalOpts::new()
            .command(&self.program)
            .root(state_dir(lease))
            .log(log_path(lease))
            .log_json();
        opts.custom_spawner(Arc::new(KillOnDrop));
        opts.build().map_err(|e| self.refused(&e))
    }

    /// Runs the container `id` of the bundle in `bundle` to its end (`crun run`: create,
    /// start and delete in one process), its stdout and stderr in the given files. The
    /// result is `crun`'s exit status. Dropping the future kills `crun`.
    pub(crate) async fn run(
        &self,
        lease: &Path,
        id: &str,
        bundle: &Path,
        stdout: std::fs::File,
        stderr: std::fs::File,
    ) -> RunEnd {
        let client = match self.client(lease) {
            Ok(client) => client,
            Err(e) => return RunEnd::Failed(e),
        };
        let opts = CreateOpts::new().io(Arc::new(FileIo { stdout, stderr }));
        match client.run(id, bundle, Some(&opts)).await {
            Ok(()) => RunEnd::Exited(0),
            Err(Error::CommandFailed { status, .. }) => RunEnd::Exited(code_of(status)),
            Err(e) => RunEnd::Failed(format!("crun run: {e}")),
        }
    }

    /// Runs `bundle` as a helper (`crun run`, output captured, nothing in a log file):
    /// `Ok` once it exits 0, otherwise what it printed.
    pub(crate) async fn run_helper(
        &self,
        state: &Path,
        id: &str,
        bundle: &Path,
    ) -> Result<(), String> {
        let log = state.with_extension("log");
        let mut opts = GlobalOpts::new()
            .command(&self.program)
            .root(state)
            .debug(true)
            .log(&log)
            .log_json();
        opts.custom_spawner(Arc::new(KillOnDrop));
        let client = opts.build().map_err(|e| self.refused(&e))?;
        match client.run(id, bundle, None).await {
            Ok(()) => Ok(()),
            Err(Error::CommandFailed {
                status,
                stdout,
                stderr,
            }) => {
                // What `crun` itself logged: the reason it failed, as opposed to the
                // helper's own words on stderr.
                let logged = std::fs::read_to_string(&log).unwrap_or_default();
                let tail = logged.len().saturating_sub(3000);
                Err(format!(
                    "crun run (helper {id}): {status}: {} {} {} [log: {}]",
                    stdout.trim(),
                    stderr.trim(),
                    crate::program::crun_words(&logged),
                    logged.get(tail..).unwrap_or_default().replace('\n', " ")
                ))
            }
            Err(e) => Err(format!("crun run (helper {id}): {e}")),
        }
    }

    /// Sends signal `signal` (a number) to the container's main process.
    pub(crate) async fn kill(&self, lease: &Path, id: &str, signal: u32) -> Result<(), String> {
        let client = self.client(lease)?;
        client
            .kill(id, signal, None)
            .await
            .map_err(|e| format!("crun kill: {e}"))
    }

    /// Deletes the container `id`, killing it first if it runs (`delete --force`); one
    /// `crun` does not know is not an error with `--force`. `crun run` deletes its
    /// container as it exits, so this finds nothing on the normal path; it is what removes
    /// the container, its cgroup and its state after a `crun` that was killed. Blocking:
    /// a dropped lease cleans up from `Drop`.
    pub(crate) fn delete_blocking(&self, lease: &Path, id: &str) -> Result<(), String> {
        let state = state_dir(lease);
        let deleted = self.blocking(&state, &["delete", "--force", id], "delete");
        // A `crun run` killed with its container (a sweep's case) deletes its own state as
        // it exits, and a `crun delete` that lost that race fails on the directory it
        // listed: with the state gone there is nothing left to delete.
        if deleted.is_err() && !state.join(id).exists() {
            return Ok(());
        }
        deleted
    }

    /// Runs the helper bundle blocking: for the clean of a lease that has files only a
    /// user namespace can delete.
    pub(crate) fn run_helper_blocking(
        &self,
        state: &Path,
        id: &str,
        bundle: &Path,
    ) -> Result<(), String> {
        let bundle = bundle.to_string_lossy();
        self.blocking(state, &["run", "--bundle", &bundle, id], "run (helper)")
    }

    /// Runs `crun --root <state> <args>` and waits: `Ok` if it exits 0, else its status
    /// and what it said on stderr.
    fn blocking(&self, state: &Path, args: &[&str], what: &str) -> Result<(), String> {
        let mut command = std::process::Command::new(&self.program);
        let output = without_credentials(&mut command)
            .arg("--root")
            .arg(state)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output();
        match output {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => Err(format!(
                "crun {what}: {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => Err(format!("run {}: {e}", self.program.display())),
        }
    }

    /// The words for a client `crun` could not be found for.
    fn refused(&self, error: &Error) -> String {
        format!("crun {}: {error}", self.program.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches `crun`'s status being read as anything but a shell reads it: an exit code
    /// as it is, a death by signal `n` as `128 + n` (what the OOM classification looks for
    /// at 137), so a killed `crun` is not an exit 0.
    #[test]
    fn a_status_is_the_exit_code_or_128_plus_the_signal() {
        assert_eq!(code_of(ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(code_of(ExitStatus::from_raw(0)), 0);
        assert_eq!(code_of(ExitStatus::from_raw(9)), 137);
        assert_eq!(code_of(ExitStatus::from_raw(15)), 143);
    }

    /// The `Io` hook `crun run` is given has nothing to close once the child has the
    /// files (the crate calls it for `create` and `exec`, not `run`).
    #[tokio::test]
    async fn the_file_io_has_nothing_to_close_after_the_start() {
        let null = || std::fs::File::open("/dev/null").expect("/dev/null");
        FileIo {
            stdout: null(),
            stderr: null(),
        }
        .close_after_start()
        .await;
    }
}
