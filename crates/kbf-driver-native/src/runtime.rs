//! [`NativeRuntime`]: one action, one lease directory, one process tree, all of it gone
//! when the lease ends. See the crate documentation for what a lease gets.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use kbf_daemon::cas::{Cas, CasError};
use kbf_daemon::tree::{
    TreeError, check_relative, fetch_message, materialize, output_paths, real_dirs,
};
use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_outputs::OutputsError;
use kbf_proto::reapi::{Action, ActionResult, Command, ExecutedActionMetadata};
use kbf_types::LeaseId;
use prost_types::Timestamp;
use tokio::process::Child;
use tokio::sync::oneshot;
use tokio::time::Instant;

use crate::cas::CasStore;
use crate::config::NativeConfig;
use crate::network::{self, Network, network_of};
use crate::procs::{self, Proc, Tracker};
use crate::user_folders::UserFolders;
use crate::xcode;

/// The driver name in the node report.
pub const DRIVER: &str = "native";
/// The lease kind this driver runs.
pub const KIND: &str = "action";

/// How long a spawn retries a program file that is still open for writing.
const BUSY_WAIT: Duration = Duration::from_secs(2);
/// The pause between two rounds of SIGKILL while ending an action's processes.
const KILL_PAUSE: Duration = Duration::from_millis(10);

/// A killer waiting for a lease's work to stop and its directory to be removed.
type Stop = oneshot::Sender<()>;

/// Runs `action` leases as process trees on this node.
#[derive(Debug)]
pub struct NativeRuntime<C> {
    config: NativeConfig,
    cas: Arc<C>,
    stops: Mutex<BTreeMap<LeaseId, oneshot::Sender<Stop>>>,
    /// The sandbox rules for the user folders (empty without them).
    rules: String,
}

/// Everything `prepare` works out for `execute` and `finish`.
struct Prepared {
    /// The lease directory's real path, every link resolved: the only place (with
    /// `/dev`) the sandbox lets the action write.
    lease: PathBuf,
    /// The lease's copy of the input root.
    root: PathBuf,
    work_dir: PathBuf,
    working_directory: String,
    program: PathBuf,
    args: Vec<String>,
    /// The lease's own directories' variables ([`crate::home`]), set before `env`.
    lease_env: Vec<(&'static str, OsString)>,
    env: Vec<(String, String)>,
    outputs: Vec<String>,
    timeout: Duration,
    network: Network,
    /// The Xcode the action names (`xcode`), as its `DEVELOPER_DIR`.
    developer_dir: Option<PathBuf>,
}

impl<C: Cas> NativeRuntime<C> {
    /// A runtime that reads and writes blobs through `cas`. Makes the scratch
    /// directory, and removes any lease directory a previous daemon left in it.
    ///
    /// # Errors
    /// The scratch directory cannot be made or read. A leftover that cannot be
    /// removed is moved aside instead (see the crate documentation), not an error.
    pub fn new(config: NativeConfig, cas: Arc<C>) -> std::io::Result<Self> {
        Self::with_remover(config, cas, &kbf_outputs::remove_tree)
    }

    /// [`NativeRuntime::new`], sweeping the scratch root with `remove`: a test hands
    /// it a remover that fails, as a directory an action locked would.
    fn with_remover(
        config: NativeConfig,
        cas: Arc<C>,
        remove: &dyn Fn(&Path) -> std::io::Result<()>,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&config.scratch)?;
        crate::sweep::sweep(&config.scratch, remove)?;
        let rules = config
            .user_folders
            .as_ref()
            .map(UserFolders::rules)
            .unwrap_or_default();
        let runtime = Self {
            config,
            cas,
            stops: Mutex::new(BTreeMap::new()),
            rules,
        };
        sweep_user_folders(&runtime.config, remove);
        Ok(runtime)
    }

    /// The node report entries this driver adds: how it keeps the network off, and
    /// one `xcode` entry per Xcode build it can select.
    #[must_use]
    pub fn capabilities(&self) -> Vec<(String, String)> {
        let mut entries = vec![(
            network::CAPABILITY.to_owned(),
            self.config.isolation.name().to_owned(),
        )];
        entries.extend(
            self.config
                .xcodes
                .keys()
                .map(|build| (xcode::CAPABILITY.to_owned(), build.clone())),
        );
        entries
    }

    fn stops(&self) -> MutexGuard<'_, BTreeMap<LeaseId, oneshot::Sender<Stop>>> {
        // Each update is one map operation, so the map is consistent after a panic.
        self.stops.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fetches and checks the action, and writes its inputs into `dir`.
    async fn prepare(&self, work: &Work, dir: &Path) -> Result<Prepared, RuntimeError> {
        let cas = &*self.cas;
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
        let Some((program, args)) = command.arguments.split_first() else {
            return Err(RuntimeError::Invalid(
                "the Command has no arguments".to_owned(),
            ));
        };
        check_relative("working directory", &command.working_directory).map_err(tree_error)?;
        let outputs = output_paths(&command).map_err(tree_error)?;
        let timeout = timeout_of(&action, self.config.default_timeout)?;
        let network = network_of(&action, &command)?;
        let developer_dir = xcode::developer_dir(&self.config.xcodes, &action, &command)?;

        tokio::fs::create_dir(dir).await.map_err(failed(dir))?;
        let lease = tokio::fs::canonicalize(dir).await.map_err(failed(dir))?;
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
        let lease_env = crate::home::make(dir).await.map_err(failed(dir))?;
        let env: Vec<(String, String)> = command
            .environment_variables
            .iter()
            .map(|v| (v.name.clone(), v.value.clone()))
            .collect();
        let program = resolve(program, &work_dir, &env)
            .map_err(|e| RuntimeError::Failed(format!("{program}: {e}")))?;
        Ok(Prepared {
            lease,
            root,
            work_dir,
            working_directory: command.working_directory.clone(),
            program,
            args: args.to_vec(),
            lease_env,
            env,
            outputs,
            timeout,
            network,
            developer_dir,
        })
    }

    /// Runs the prepared action until it exits, times out, outgrows its memory limit
    /// or is killed, then ends every process of it. The exit code of an action that
    /// exited.
    async fn execute(
        &self,
        work: &Work,
        dir: &Path,
        prepared: &Prepared,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<i32, RuntimeError> {
        let stdout_path = dir.join("stdout");
        let stderr_path = dir.join("stderr");
        let stdout = std::fs::File::create(&stdout_path).map_err(failed(&stdout_path))?;
        let stderr = std::fs::File::create(&stderr_path).map_err(failed(&stderr_path))?;
        let (program, args) = self.config.isolation.wrap(
            prepared.network,
            &prepared.lease,
            &self.rules,
            prepared.program.clone(),
            &prepared.args,
        );
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&args)
            .current_dir(&prepared.work_dir)
            .env_clear()
            .envs(prepared.lease_env.iter().map(|(k, v)| (k, v)))
            .envs(prepared.env.iter().map(|(k, v)| (k, v)))
            .envs(
                prepared
                    .developer_dir
                    .iter()
                    .map(|d| (xcode::DEVELOPER_DIR, d)),
            )
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0)
            .kill_on_drop(true);
        let (mut child, leader) = spawn(&mut command)
            .await
            .map_err(failed(&prepared.program))?;
        let me = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
        let mut group = Group::new(Tracker::new(leader, me));
        let limit = self.config.memory.limit(work.resources.memory_bytes);
        let deadline = Instant::now() + prepared.timeout;
        let mut tick = tokio::time::interval(self.config.poll);
        // A slow measurement delays the next one rather than bunching them up.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let ended = loop {
            tokio::select! {
                status = child.wait() => break Ended::Exited(status),
                () = tokio::time::sleep_until(deadline) => break Ended::TimedOut,
                Ok(by) = &mut *stop => {
                    *killer = Some(by);
                    break Ended::Killed;
                }
                _ = tick.tick() => {
                    let used = group.measure().await?;
                    if let Some(limit) = limit && used > limit {
                        break Ended::OutOfMemory { used, limit };
                    }
                }
            }
        };
        group.end(&mut child, self.config.kill_wait).await?;
        match ended {
            Ended::Exited(status) => Ok(exit_code(status.map_err(failed(&program))?)),
            Ended::TimedOut => Err(RuntimeError::TimedOut),
            Ended::Killed => Err(RuntimeError::Killed),
            Ended::OutOfMemory { used, limit } => {
                tracing::warn!(lease = %work.lease_id, used, limit, "killed past its memory limit");
                Err(RuntimeError::OutOfMemory { used, limit })
            }
        }
    }

    /// Every step but the clean.
    async fn attempt(
        &self,
        work: &Work,
        dir: &Path,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<ActionResult, RuntimeError> {
        let mut clock = ExecutedActionMetadata {
            worker_start_timestamp: now(),
            input_fetch_start_timestamp: now(),
            ..ExecutedActionMetadata::default()
        };
        // A kill during prepare (a long input fetch) stops it at once; the clean
        // removes whatever was written.
        let prepared = tokio::select! {
            prepared = self.prepare(work, dir) => prepared?,
            Ok(by) = &mut *stop => {
                *killer = Some(by);
                return Err(RuntimeError::Killed);
            }
        };
        clock.input_fetch_completed_timestamp = now();
        clock.execution_start_timestamp = now();
        let exit_code = self.execute(work, dir, &prepared, stop, killer).await?;
        clock.execution_completed_timestamp = now();

        clock.output_upload_start_timestamp = now();
        let store = CasStore(Arc::clone(&self.cas));
        let mut result = ActionResult {
            exit_code,
            ..ActionResult::default()
        };
        // From the input root: the action may have replaced its working directory.
        kbf_outputs::collect(
            &store,
            &prepared.root,
            &prepared.working_directory,
            &prepared.outputs,
            self.config.outputs,
            &mut result,
        )
        .await
        .map_err(outputs_error)?;
        let max = self.config.outputs.max_stdio_bytes;
        for (name, slot) in [
            ("stdout", &mut result.stdout_digest),
            ("stderr", &mut result.stderr_digest),
        ] {
            let stored = kbf_outputs::store_file(&store, &dir.join(name), max).await;
            *slot = Some(stored.map_err(outputs_error)?);
        }
        clock.output_upload_completed_timestamp = now();
        clock.worker_completed_timestamp = now();
        result.execution_metadata = Some(clock);
        Ok(result)
    }
}

impl<C: Cas> Runtime for NativeRuntime<C> {
    fn driver(&self) -> &'static str {
        DRIVER
    }

    fn serves(&self, kind: &str) -> bool {
        kind == KIND
    }

    async fn run(&self, work: Work) -> Result<ActionResult, RuntimeError> {
        let (stop_tx, mut stop) = oneshot::channel();
        let _registered = Registered::new(self, work.lease_id, stop_tx);
        let dir = LeaseDir {
            path: self.config.scratch.join(format!(
                "lease-{}-{}",
                work.lease_id.term, work.lease_id.seq
            )),
            armed: true,
        };
        let mut killer = None;
        let outcome = self.attempt(&work, &dir.path, &mut stop, &mut killer).await;
        let cleaned = dir.clean().await;
        sweep_user_folders_after_lease(&self.config).await;
        // A kill that arrived after the work ended still waits for the clean.
        if let Some(by) = killer.or_else(|| stop.try_recv().ok()) {
            let _ = by.send(());
        }
        match (outcome, cleaned) {
            (Ok(_), Err(why)) => Err(RuntimeError::Failed(format!("clean: {why}"))),
            (outcome, Err(why)) => {
                tracing::error!(lease = %work.lease_id, "clean: {why}");
                outcome
            }
            (outcome, Ok(())) => outcome,
        }
    }

    async fn kill(&self, lease_id: LeaseId) {
        let Some(stop) = self.stops().remove(&lease_id) else {
            return;
        };
        let (tx, rx) = oneshot::channel();
        // Answered once the work has stopped and the lease is clean. If the run ended
        // without seeing the kill, `tx` is dropped with it and `rx` returns at once.
        let _ = stop.send(tx);
        let _ = rx.await;
    }
}

/// Removes, with `remove`, the leftovers in the user folders old enough to be no
/// lease's work in progress ([`crate::user_folders`]). Not generic over the CAS, like
/// [`sweep_user_folders_after_lease`], so every test binary runs the one copy.
fn sweep_user_folders(config: &NativeConfig, remove: &dyn Fn(&Path) -> std::io::Result<()>) {
    if let Some(folders) = &config.user_folders {
        folders.sweep(config.leftover_age, SystemTime::now(), remove);
    }
}

/// [`sweep_user_folders`] after a lease, on the blocking pool. Best effort, as at
/// start: what fails is logged and tried after the next lease.
async fn sweep_user_folders_after_lease(config: &NativeConfig) {
    let Some(folders) = config.user_folders.clone() else {
        return;
    };
    let age = config.leftover_age;
    let _ = tokio::task::spawn_blocking(move || {
        folders.sweep(age, SystemTime::now(), &kbf_outputs::remove_tree);
    })
    .await;
}

/// How the watch over a running action ended.
enum Ended {
    Exited(std::io::Result<ExitStatus>),
    TimedOut,
    Killed,
    OutOfMemory { used: u64, limit: u64 },
}

/// The processes of one running action. Dropped while armed (the daemon dropped the
/// run), it kills them all, blocking.
struct Group {
    tracker: Arc<Mutex<Tracker>>,
    armed: bool,
}

impl Group {
    fn new(tracker: Tracker) -> Self {
        Self {
            tracker: Arc::new(Mutex::new(tracker)),
            armed: true,
        }
    }

    /// The live processes of the action now, on the blocking pool.
    async fn members(&self) -> Result<Vec<Proc>, RuntimeError> {
        let tracker = Arc::clone(&self.tracker);
        // A task that did not finish (a panic, a runtime shutting down) is an I/O
        // failure like the call's own.
        let members = tokio::task::spawn_blocking(move || members_now(&tracker)).await;
        members
            .map_err(std::io::Error::other)
            .and_then(|found| found)
            .map_err(failed(Path::new("process table")))
    }

    /// The memory the action's processes hold together, in bytes.
    async fn measure(&self) -> Result<u64, RuntimeError> {
        let members = self.members().await?;
        tokio::task::spawn_blocking(move || {
            members
                .iter()
                .filter_map(|p| procs::footprint(p.pid))
                .fold(0_u64, u64::saturating_add)
        })
        .await
        .map_err(std::io::Error::other)
        .map_err(failed(Path::new("memory")))
    }

    /// Ends every process of the action: SIGKILL to the group and to each process
    /// known to be the action's, again every [`KILL_PAUSE`] until a snapshot shows
    /// none alive, then reaps the leader. Processes still alive after `wait` fail the
    /// lease.
    async fn end(&mut self, child: &mut Child, wait: Duration) -> Result<(), RuntimeError> {
        let give_up = Instant::now() + wait;
        loop {
            let members = self.members().await?;
            if members.is_empty() {
                break;
            }
            procs::kill_all(&lock(&self.tracker), &members);
            if Instant::now() >= give_up {
                let pids: Vec<String> = members.iter().map(|p| p.pid.to_string()).collect();
                return Err(RuntimeError::Failed(format!(
                    "processes of the action survived SIGKILL for {wait:?}: {}",
                    pids.join(", ")
                )));
            }
            tokio::time::sleep(KILL_PAUSE).await;
        }
        self.armed = false;
        // The leader is dead; reap it if it is not reaped yet.
        child
            .wait()
            .await
            .map_err(failed(Path::new("reap the action")))?;
        Ok(())
    }
}

fn lock(tracker: &Mutex<Tracker>) -> MutexGuard<'_, Tracker> {
    // A tracker update is a few map operations on data a panic would not leave half
    // written in a way that matters: at worst a process is followed a poll longer.
    tracker.lock().unwrap_or_else(PoisonError::into_inner)
}

fn members_now(tracker: &Mutex<Tracker>) -> std::io::Result<Vec<Proc>> {
    let snapshot = procs::snapshot()?;
    Ok(lock(tracker).members(&snapshot))
}

impl Drop for Group {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let ended = (0..100).any(|_| {
            let members = members_now(&self.tracker).unwrap_or_default();
            procs::kill_all(&lock(&self.tracker), &members);
            std::thread::sleep(KILL_PAUSE);
            members.is_empty()
        });
        tracing::warn!(ended, "run dropped; killed the action's processes");
    }
}

/// Spawns `command`, retrying while its program is busy: a just-written input file is
/// briefly held open for writing by any child another thread forks before it execs
/// (ETXTBSY). Returns the child and its pid.
async fn spawn(command: &mut tokio::process::Command) -> std::io::Result<(Child, i32)> {
    let give_up = Instant::now() + BUSY_WAIT;
    loop {
        match command.spawn() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < give_up => {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            spawned => {
                let child = spawned?;
                // A child that has not been waited for always has its pid.
                let pid = child.id().and_then(|pid| i32::try_from(pid).ok());
                let pid = pid.ok_or(std::io::Error::other("the child has no pid"))?;
                return Ok((child, pid));
            }
        }
    }
}

/// Where `PATH` points when the Command sets none: what `execvp` searches then.
const DEFAULT_PATH: &str = "/usr/bin:/bin";

/// The program to run, REAPI v2.3's way: a path with a slash is relative to the
/// working directory; a bare name is looked up in the Command's `PATH` (relative
/// entries from the working directory too). Resolved here, not by `execvp`, so the
/// sandbox wrapper is handed a path and a missing program fails the lease the same way
/// with or without it.
fn resolve(program: &str, work_dir: &Path, env: &[(String, String)]) -> std::io::Result<PathBuf> {
    if program.contains('/') {
        let path = work_dir.join(program);
        return executable(&path).then_some(path).ok_or_else(not_found);
    }
    let path = env
        .iter()
        .rev()
        .find(|(name, _)| name == "PATH")
        .map_or(DEFAULT_PATH, |(_, value)| value.as_str());
    path.split(':')
        .map(|dir| work_dir.join(dir).join(program))
        .find(|candidate| executable(candidate))
        .ok_or_else(not_found)
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn not_found() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no such executable file (searched as REAPI v2.3 says)",
    )
}

/// The exit status, or 128 plus the signal number for a process a signal ended (the
/// shell's convention).
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// The lease's directory, removed when the lease ends; by `Drop` if the run is
/// dropped first.
struct LeaseDir {
    path: PathBuf,
    armed: bool,
}

impl LeaseDir {
    /// Removes the directory, off the async threads.
    async fn clean(mut self) -> Result<(), String> {
        self.armed = false;
        let path = self.path.clone();
        let cleaned = tokio::task::spawn_blocking(move || clean(&path)).await;
        cleaned.map_err(clean_task_failed)?
    }
}

/// The clean's error when its blocking task did not finish (it panicked, or the
/// runtime shut down before it ran).
fn clean_task_failed(error: tokio::task::JoinError) -> String {
    format!("clean task: {error}")
}

fn clean(path: &Path) -> Result<(), String> {
    kbf_outputs::remove_tree(path).map_err(|e| format!("remove {}: {e}", path.display()))
}

impl Drop for LeaseDir {
    fn drop(&mut self) {
        if self.armed {
            let cleaned = clean(&self.path);
            tracing::warn!(dir = %self.path.display(), ?cleaned, "run dropped; cleaned up");
        }
    }
}

/// A lease's entry in the kill table, removed when its run ends or is dropped.
struct Registered<'a, C> {
    runtime: &'a NativeRuntime<C>,
    id: LeaseId,
}

impl<'a, C: Cas> Registered<'a, C> {
    fn new(runtime: &'a NativeRuntime<C>, id: LeaseId, stop: oneshot::Sender<Stop>) -> Self {
        runtime.stops().insert(id, stop);
        Self { runtime, id }
    }
}

impl<C> Drop for Registered<'_, C> {
    fn drop(&mut self) {
        self.runtime
            .stops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

fn now() -> Option<Timestamp> {
    Some(Timestamp::from(SystemTime::now()))
}

/// The action's timeout, or `default` when it names none.
fn timeout_of(action: &Action, default: Duration) -> Result<Duration, RuntimeError> {
    let Some(timeout) = &action.timeout else {
        return Ok(default);
    };
    let (Ok(seconds), Ok(nanos)) = (u64::try_from(timeout.seconds), u32::try_from(timeout.nanos))
    else {
        return Err(RuntimeError::Invalid(
            "the action's timeout is negative".to_owned(),
        ));
    };
    let timeout = Duration::new(seconds, nanos);
    Ok(if timeout.is_zero() { default } else { timeout })
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

/// An [`OutputsError`] as the lease reports it: a bad output path or a name that is
/// not UTF-8 is the action's error (running it again would fail the same way); a
/// limit, a failed read or a failed upload fails the lease, as the container driver
/// reports them.
fn outputs_error(error: OutputsError) -> RuntimeError {
    match error {
        OutputsError::Invalid(why) => RuntimeError::Invalid(why),
        other => RuntimeError::Failed(other.to_string()),
    }
}

/// An I/O failure at `path` as the lease reports it: the farm's.
fn failed(path: &Path) -> impl FnOnce(std::io::Error) -> RuntimeError + '_ {
    move |error| RuntimeError::Failed(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches an action's timeout being ignored, a zero timeout meaning "no time at
    /// all", and a negative one wrapping to centuries.
    #[test]
    fn the_timeout_is_the_actions_or_the_default() {
        let default = Duration::from_secs(60);
        let with = |seconds, nanos| Action {
            timeout: Some(prost_types::Duration { seconds, nanos }),
            ..Action::default()
        };
        assert_eq!(timeout_of(&Action::default(), default).ok(), Some(default));
        assert_eq!(timeout_of(&with(0, 0), default).ok(), Some(default));
        assert_eq!(
            timeout_of(&with(2, 500), default).ok(),
            Some(Duration::new(2, 500))
        );
        assert!(matches!(
            timeout_of(&with(-1, 0), default),
            Err(RuntimeError::Invalid(_))
        ));
    }

    /// Catches a clean whose blocking task panicked reported without saying it was the
    /// clean that failed (the lease then fails INTERNAL with this text).
    #[tokio::test]
    async fn a_clean_task_that_did_not_finish_names_the_clean() {
        let panicked = tokio::task::spawn_blocking(|| panic!("clean panicked"))
            .await
            .expect_err("the task panicked");
        assert!(clean_task_failed(panicked).starts_with("clean task: "));
    }

    /// A CAS no test here reaches.
    struct NoCas;

    impl Cas for NoCas {
        async fn get(&self, digest: &kbf_proto::reapi::Digest) -> Result<Vec<u8>, CasError> {
            Err(CasError::Missing(digest.hash.clone()))
        }

        async fn put(&self, _: Vec<u8>) -> Result<kbf_proto::reapi::Digest, CasError> {
            Err(CasError::Unavailable("-".into(), "no CAS".into()))
        }
    }

    /// Catches the runtime refusing to start over a leftover lease directory it
    /// cannot remove (one build step would take the node out of the farm): the start
    /// goes on, and the leftover is out of the lease names' way.
    #[tokio::test]
    async fn a_start_goes_on_past_a_leftover_that_will_not_go() {
        let scratch = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("start-{}", std::process::id()));
        let _ = kbf_outputs::remove_tree(&scratch);
        std::fs::create_dir_all(scratch.join("lease-3-3/root")).expect("mkdir");
        let refuse = |_: &Path| Err(std::io::Error::from_raw_os_error(libc::EPERM));
        let config = NativeConfig::new(scratch.clone());
        let started = NativeRuntime::with_remover(config, Arc::new(NoCas), &refuse);
        let started = started.expect("the start goes on");
        let digest = kbf_proto::reapi::Digest::default();
        assert!(started.cas.get(&digest).await.is_err(), "no CAS here");
        assert!(started.cas.put(Vec::new()).await.is_err(), "no CAS here");
        assert!(!scratch.join("lease-3-3").exists(), "moved out of the way");
        let aside = std::fs::read_dir(scratch.join(crate::sweep::QUARANTINE))
            .expect("quarantine")
            .count();
        assert_eq!(aside, 1);
        kbf_outputs::remove_tree(&scratch).expect("clean");
    }

    /// Catches a signal death reported as exit 0 or as the raw status.
    #[test]
    fn a_signal_is_128_plus_its_number() {
        assert_eq!(exit_code(ExitStatus::from_raw(3 << 8)), 3);
        assert_eq!(exit_code(ExitStatus::from_raw(libc::SIGKILL)), 128 + 9);
    }

    /// Catches errors reported as the wrong party's: a missing input or a bad path is
    /// the client's, anything else the farm's. (Compared as text: a pattern guard here
    /// would be a branch the coverage ratchet counts and no run takes.)
    #[test]
    fn errors_are_reported_as_whose_they_are() {
        let shown = |e: RuntimeError| format!("{e:?}");
        let missing = tree_error(TreeError::Cas(CasError::Missing("ab/1".to_owned())));
        assert_eq!(shown(missing), "MissingBlob(\"ab/1\")");
        let corrupt = tree_error(TreeError::Cas(CasError::Corrupt("a".into(), "b".into())));
        assert_eq!(
            shown(corrupt),
            "Failed(\"blob a failed verification: its bytes hash to b\")"
        );
        assert_eq!(
            shown(tree_error(TreeError::Invalid("x".into()))),
            "Invalid(\"x\")"
        );
        assert_eq!(
            shown(outputs_error(OutputsError::Invalid("y".into()))),
            "Invalid(\"y\")"
        );
        let io = OutputsError::Io {
            path: PathBuf::from("p"),
            source: std::io::Error::other("disk"),
        };
        assert_eq!(shown(outputs_error(io)), "Failed(\"p: disk\")");
    }

    /// Catches a bare program name not looked up in the Command's PATH (or looked up in
    /// the daemon's), a relative PATH entry not taken from the working directory, the
    /// default PATH not used when the Command sets none, and a path that is not an
    /// executable file accepted.
    #[test]
    fn programs_resolve_as_reapi_says() {
        let wd = Path::new("/nonexistent/wd");
        let env = |path: &str| vec![("PATH".to_owned(), path.to_owned())];
        assert_eq!(
            resolve("sh", wd, &env("/nonexistent:/bin")).expect("in PATH"),
            Path::new("/bin/sh")
        );
        assert_eq!(
            resolve("env", wd, &[]).expect("default PATH"),
            Path::new("/usr/bin/env")
        );
        assert!(resolve("sh", wd, &env("/nonexistent")).is_err());
        assert!(
            resolve("sh", wd, &env("bin")).is_err(),
            "relative to the working directory"
        );
        assert_eq!(
            resolve("/bin/sh", wd, &[]).expect("absolute"),
            Path::new("/bin/sh")
        );
        assert!(resolve("./tool", wd, &[]).is_err());
        assert!(resolve("/", wd, &[]).is_err(), "a directory");
        let err = resolve("/etc/hosts", wd, &[]).expect_err("not executable");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// Catches a spawn that gives up at once on a program still open for writing (a
    /// just-materialised input another fork briefly holds), instead of retrying.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_busy_program_is_retried() {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let program = dir.join(format!("busy-{}", std::process::id()));
        let _ = std::fs::remove_file(&program);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&program)
            .expect("create");
        file.write_all(b"#!/bin/sh\nexit 7\n").expect("write");
        let closer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            drop(file);
        });
        let mut command = tokio::process::Command::new(&program);
        let started = Instant::now();
        let (mut child, pid) = spawn(&mut command).await.expect("spawned once closed");
        assert!(started.elapsed() >= Duration::from_millis(50), "it waited");
        assert!(pid > 0);
        assert_eq!(child.wait().await.expect("wait").code(), Some(7));
        closer.await.expect("closer");
        // Held open for writing past the wait: the spawn gives up with ETXTBSY.
        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(&program)
            .expect("open");
        let busy = spawn(&mut command).await.expect_err("still busy");
        assert_eq!(busy.raw_os_error(), Some(libc::ETXTBSY));
        drop(held);
        // Any other failure is not retried: a file that is no program at all.
        std::fs::write(&program, [0x7f, b'E', b'L', b'F', 0, 0]).expect("write");
        let started = Instant::now();
        let error = spawn(&mut command).await.expect_err("not a program");
        assert_ne!(error.raw_os_error(), Some(libc::ETXTBSY));
        assert!(started.elapsed() < BUSY_WAIT);
        std::fs::remove_file(&program).expect("remove");
    }
}
