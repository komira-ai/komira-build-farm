//! [`LocalRuntime`]: **for tests only.** Runs each action as a plain child process of
//! the daemon, so the daemon's execution path (fetch, materialise, run, measure,
//! upload, report) can be tested end to end where no container driver is available.
//!
//! It isolates nothing: the action runs as the daemon's user, with the daemon's
//! filesystem, network and limits, and a process it leaves behind keeps running (so it
//! breaks the rule [`crate::tree::collect`] asks of a runtime, that no process of the
//! action is left when the outputs are read). No timeout is applied. Farm nodes run actions through the container driver
//! (`kbf-driver-container`); the `kbf-daemon` binary does not offer this runtime.
//!
//! One lease, in order: fetch the Action and Command and check them; write the input
//! root into a fresh lease directory; run `arguments` with the Command's environment
//! only, in the working directory under the input root; on exit, read the outputs,
//! stdout and stderr into the CAS; remove the lease directory. The `ActionResult`
//! carries the exit code, REAPI's execution timestamps, and a `kbf.worker.v1`
//! `ResourceUsage` measured by the kernel ([`crate::usage`]). A lease directory that cannot be removed
//! fails the lease: a dirty node must be loud.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use kbf_proto::reapi::{Action, ActionResult, Command, ExecutedActionMetadata};
use kbf_types::LeaseId;
use prost_types::Timestamp;
use tokio::sync::oneshot;

use crate::cas::{Cas, CasError};
use crate::runtime::{Runtime, RuntimeError, Work};
use crate::tree::{
    TreeError, check_relative, collect, fetch_message, materialize, output_paths, real_dirs,
};
use crate::usage::{Child, usage_any};

/// The driver name in the node report.
pub const LOCAL_DRIVER: &str = "local";

/// Runs `action` leases as child processes of the daemon. **Tests only**: see the
/// module documentation.
#[derive(Debug)]
pub struct LocalRuntime<C> {
    cas: Arc<C>,
    scratch: PathBuf,
    stops: Mutex<BTreeMap<LeaseId, oneshot::Sender<()>>>,
}

/// REAPI's execution timestamps, in the order a lease reaches them.
#[derive(Default)]
struct Clock(ExecutedActionMetadata);

fn now() -> Option<Timestamp> {
    Some(Timestamp::from(SystemTime::now()))
}

impl<C: Cas> LocalRuntime<C> {
    /// A runtime that reads and writes blobs through `cas` and makes each lease's
    /// directory under `scratch`.
    #[must_use]
    pub fn new(cas: Arc<C>, scratch: PathBuf) -> Self {
        Self {
            cas,
            scratch,
            stops: Mutex::new(BTreeMap::new()),
        }
    }

    fn stops(&self) -> MutexGuard<'_, BTreeMap<LeaseId, oneshot::Sender<()>>> {
        // Each update is one map operation, so the map is consistent after a panic.
        self.stops.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the lease's directory `dir`. One that already exists is not this run's
    /// to use or to remove, so it fails the lease.
    async fn make_dir(&self, dir: &Path) -> Result<(), RuntimeError> {
        tokio::fs::create_dir_all(&self.scratch)
            .await
            .map_err(failed(&self.scratch))?;
        tokio::fs::create_dir(dir).await.map_err(failed(dir))
    }

    /// Everything but the clean-up.
    async fn attempt(
        &self,
        work: &Work,
        dir: &Path,
        stop: oneshot::Receiver<()>,
    ) -> Result<ActionResult, RuntimeError> {
        let cas = &*self.cas;
        let mut clock = Clock::default();
        clock.0.worker_start_timestamp = now();
        clock.0.input_fetch_start_timestamp = now();
        let action: Action = fetch_message(cas, &work.action_digest)
            .await
            .map_err(tree_error)?;
        let command_digest = action
            .command_digest
            .as_ref()
            .ok_or_else(|| RuntimeError::Invalid("the Action names no Command".to_owned()))?;
        let input_root = action
            .input_root_digest
            .as_ref()
            .ok_or_else(|| RuntimeError::Invalid("the Action names no input root".to_owned()))?;
        let command: Command = fetch_message(cas, command_digest)
            .await
            .map_err(tree_error)?;
        let Some(program) = command.arguments.first() else {
            return Err(RuntimeError::Invalid(
                "the Command has no arguments".to_owned(),
            ));
        };
        check_relative("working directory", &command.working_directory).map_err(tree_error)?;
        let outputs = output_paths(&command).map_err(tree_error)?;

        let root = dir.join("root");
        tokio::fs::create_dir(&root).await.map_err(failed(&root))?;
        materialize(cas, input_root, &root)
            .await
            .map_err(tree_error)?;
        // REAPI: the worker makes the working directory and each output's parent.
        let work_dir = real_dirs(&root, &command.working_directory)
            .await
            .map_err(tree_error)?;
        for output in &outputs {
            let parent = output.rsplit_once('/').map_or("", |(parent, _)| parent);
            real_dirs(&work_dir, parent).await.map_err(tree_error)?;
        }
        clock.0.input_fetch_completed_timestamp = now();

        let stdout_path = dir.join("stdout");
        let stderr_path = dir.join("stderr");
        let stdout = std::fs::File::create(&stdout_path).map_err(failed(&stdout_path))?;
        let stderr = std::fs::File::create(&stderr_path).map_err(failed(&stderr_path))?;
        // REAPI v2.3 (the newest version kbf-front advertises): a program path with a
        // slash is relative to the working directory, and a bare name is looked up in
        // the Command's PATH. (v2.2 and older resolved it from the input root.) The
        // join is explicit because `Command::current_dir` leaves it unspecified which
        // directory a relative program path is resolved from.
        let program = if program.contains('/') {
            work_dir.join(program)
        } else {
            PathBuf::from(program)
        };
        let mut process = Process::new(&program);
        process
            .args(&command.arguments[1..])
            .current_dir(&work_dir)
            .env_clear()
            .envs(
                command
                    .environment_variables
                    .iter()
                    .map(|v| (&v.name, &v.value)),
            )
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr);
        clock.0.execution_start_timestamp = now();
        let child = Child::spawn(process).await.map_err(failed(&program))?;
        let stopped = async {
            // The sender is dropped only after `run` is done with this receiver.
            let _ = stop.await;
        };
        let Some(exited) = child.wait(stopped).await.map_err(failed(&program))? else {
            return Err(RuntimeError::Killed);
        };
        clock.0.execution_completed_timestamp = now();

        clock.0.output_upload_start_timestamp = now();
        let mut result = ActionResult {
            exit_code: exited.exit_code,
            ..ActionResult::default()
        };
        // From the input root: the action may have replaced its working directory.
        collect(
            cas,
            &root,
            &command.working_directory,
            &outputs,
            &mut result,
        )
        .await
        .map_err(tree_error)?;
        for (path, slot) in [
            (&stdout_path, &mut result.stdout_digest),
            (&stderr_path, &mut result.stderr_digest),
        ] {
            let bytes = tokio::fs::read(path).await.map_err(failed(path))?;
            let digest = cas.put(bytes).await.map_err(TreeError::from);
            *slot = Some(digest.map_err(tree_error)?);
        }
        clock.0.output_upload_completed_timestamp = now();
        clock.0.worker_completed_timestamp = now();
        clock.0.auxiliary_metadata = vec![usage_any(&exited.usage)];
        result.execution_metadata = Some(clock.0);
        Ok(result)
    }
}

/// A [`TreeError`] as the lease reports it: a missing blob is the client's to upload,
/// a broken rule is the client's error, and anything else is the farm's.
fn tree_error(error: TreeError) -> RuntimeError {
    match error {
        TreeError::Cas(CasError::Missing(blob)) => RuntimeError::MissingBlob(blob),
        TreeError::Invalid(why) => RuntimeError::Invalid(why),
        other => RuntimeError::Failed(other.to_string()),
    }
}

/// An I/O failure at `path` as the lease reports it: the farm's.
fn failed(path: &Path) -> impl FnOnce(std::io::Error) -> RuntimeError + '_ {
    move |error| RuntimeError::Failed(format!("{}: {error}", path.display()))
}

impl<C: Cas> Runtime for LocalRuntime<C> {
    fn driver(&self) -> &'static str {
        LOCAL_DRIVER
    }

    fn serves(&self, kind: &str) -> bool {
        kind == "action"
    }

    async fn run(&self, work: Work) -> Result<ActionResult, RuntimeError> {
        let (tx, stop) = oneshot::channel();
        self.stops().insert(work.lease_id, tx);
        let dir = self.scratch.join(format!(
            "lease-{}-{}",
            work.lease_id.term, work.lease_id.seq
        ));
        let outcome = match self.make_dir(&dir).await {
            Ok(()) => {
                let outcome = self.attempt(&work, &dir, stop).await;
                match tokio::fs::remove_dir_all(&dir).await {
                    Ok(()) => outcome,
                    Err(e) => {
                        tracing::error!(lease = %work.lease_id, "lease directory left behind: {e}");
                        Err(RuntimeError::Failed(format!(
                            "the lease directory {} could not be removed: {e}",
                            dir.display()
                        )))
                    }
                }
            }
            Err(e) => Err(e),
        };
        self.stops().remove(&work.lease_id);
        outcome
    }

    async fn kill(&self, lease_id: LeaseId) {
        if let Some(stop) = self.stops().remove(&lease_id) {
            // The receiver is gone only if the run already ended.
            let _ = stop.send(());
        }
    }
}
