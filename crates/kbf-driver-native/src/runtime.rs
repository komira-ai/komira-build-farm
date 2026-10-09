//! [`NativeRuntime`]: one action, one lease directory, one process tree, all of it gone
//! when the lease ends. See the crate documentation for what a lease gets.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use kbf_daemon::cas::{Cas, CasError};
use kbf_daemon::tree::{
    TreeError, check_relative, fetch_message, materialize, output_paths, real_dirs,
};
use kbf_daemon::{DriverReport, Runtime, RuntimeError, Work};
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
use crate::record::Exec;
use crate::user_folders::UserFolders;
use crate::xcode;

/// The driver name in the node report.
pub const DRIVER: &str = "native";
/// The lease kind this driver runs.
pub const KIND: &str = "action";

/// The directory under the scratch root the `xcrun` warm-up runs in, as its lease
/// directory ([`NativeRuntime::warm_xcrun`]): a `lease-` name, so the next start
/// sweeps one a killed daemon left. The warm-up of each later survey's newly ready
/// Xcodes ([`NativeRuntime::apply_xcodes`]) runs in `<this>-<n>` (`n` from 2), so two
/// that overlap never share, or remove, each other's directory.
pub const WARM_UP_DIR: &str = "lease-warm-up";

/// How long a spawn retries a program file that is still open for writing.
const BUSY_WAIT: Duration = Duration::from_secs(2);
/// The pause between two rounds of SIGKILL while ending an action's processes.
pub(crate) const KILL_PAUSE: Duration = Duration::from_millis(10);

/// A killer waiting for a lease's work to stop and its directory to be removed.
type Stop = oneshot::Sender<()>;

/// Runs `action` leases as process trees on this node.
#[derive(Debug)]
pub struct NativeRuntime<C> {
    config: NativeConfig,
    cas: Arc<C>,
    stops: Mutex<BTreeMap<LeaseId, oneshot::Sender<Stop>>>,
    /// The Xcodes an action may name: `config.xcodes` at first, then the ready ones of
    /// each survey ([`NativeRuntime::apply_xcodes`]).
    xcodes: RwLock<BTreeMap<String, PathBuf>>,
    /// The sandbox rules for the user folders (empty without them).
    rules: String,
    /// How `xcrun` is warmed, once [`NativeRuntime::warm_xcrun`] was called: then each
    /// survey that makes an Xcode ready warms it too. Locked before `xcodes`.
    warm_up: Mutex<Option<WarmUp>>,
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
    /// directory; kills every action a previous daemon recorded there and left running
    /// ([`crate::record`]); and removes every lease directory it left. All of it is done
    /// when this returns, so before the daemon says `Hello`.
    ///
    /// # Errors
    /// The scratch directory cannot be made or read, or a recorded action's processes
    /// survived SIGKILL for the kill wait. A leftover directory that cannot be removed
    /// is moved aside instead (see the crate documentation), not an error.
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
        crate::sweep::sweep(
            &config.scratch,
            remove,
            config.kill_wait,
            &crate::sweep::SYSTEM,
        )?;
        // After the sweep, which sets aside a `runs` that is not the daemon's directory.
        let mut runs = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(runs.recursive(true), 0o700)
            .create(config.scratch.join(crate::record::RUNS))?;
        let rules = config
            .user_folders
            .as_ref()
            .map(UserFolders::rules)
            .unwrap_or_default();
        let runtime = Self {
            xcodes: RwLock::new(config.xcodes.clone()),
            config,
            cas,
            stops: Mutex::new(BTreeMap::new()),
            rules,
            warm_up: Mutex::new(None),
        };
        sweep_user_folders(&runtime.config);
        Ok(runtime)
    }

    /// Starts the `xcrun` warm-up for the node's own Xcode and the Xcodes actions may
    /// name now on a thread of its own ([`xcode::warm`]), each lookup given `within`,
    /// run as an action is: under this node's isolation with the network off, the
    /// user-folder rules and [`WARM_UP_DIR`] as its lease directory. From then on each
    /// survey that makes an Xcode ready warms that one the same way
    /// ([`Self::apply_xcodes`]). So the daemon runs no `xcrun` lookup outside the
    /// sandbox but its survey's, which does not read the cache
    /// ([`xcode::NO_CACHE`]), and `xcrun` still fills its cache. `None` where `xcrun`
    /// is not a file or the directory cannot be made.
    #[must_use]
    pub fn warm_xcrun(
        &self,
        xcrun: &Path,
        within: Duration,
    ) -> Option<std::thread::JoinHandle<()>> {
        let mut warm_up = self.warm_up();
        let dirs = std::iter::once(None)
            .chain(self.xcodes().values().cloned().map(Some))
            .collect();
        *warm_up = Some(WarmUp {
            xcrun: xcrun.to_owned(),
            within,
            started: 1,
        });
        warm_xcrun(&self.config, &self.rules, WARM_UP_DIR, xcrun, dirs, within)
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
            self.xcodes()
                .keys()
                .map(|build| (xcode::CAPABILITY.to_owned(), build.clone())),
        );
        entries
    }

    /// Makes the ready Xcodes of `installed` the ones actions may name, and returns
    /// what the daemon reports for them: [`Self::capabilities`] (one `xcode` entry per
    /// ready build), and every installed Xcode with its state for `NodeStatus`. Once
    /// [`Self::warm_xcrun`] was called, starts the warm-up of each Xcode this makes
    /// ready (by `DEVELOPER_DIR`), so the first action to name it does not pay for an
    /// uncached lookup per tool.
    #[must_use]
    pub fn apply_xcodes(&self, installed: &[xcode::Xcode]) -> DriverReport {
        self.apply_and_warm(installed).0
    }

    /// [`Self::apply_xcodes`], and the warm-up it started, if any.
    fn apply_and_warm(
        &self,
        installed: &[xcode::Xcode],
    ) -> (DriverReport, Option<std::thread::JoinHandle<()>>) {
        let ready = xcode::ready(installed);
        let mut warm_up = self.warm_up();
        let newly: Vec<Option<PathBuf>> = {
            let before = self.xcodes();
            let was_ready = |dir: &PathBuf| before.values().any(|known| known == dir);
            ready
                .values()
                .filter(|dir| !was_ready(dir))
                .cloned()
                .map(Some)
                .collect()
        };
        // The map is replaced whole, so it is consistent after a panic.
        *self.xcodes.write().unwrap_or_else(PoisonError::into_inner) = ready;
        let warming = match warm_up.as_mut() {
            Some(warm) if !newly.is_empty() => {
                warm.started += 1;
                let dir = format!("{WARM_UP_DIR}-{}", warm.started);
                warm_xcrun(
                    &self.config,
                    &self.rules,
                    &dir,
                    &warm.xcrun,
                    newly,
                    warm.within,
                )
            }
            _ => None,
        };
        drop(warm_up);
        let report = DriverReport {
            entries: self.capabilities(),
            xcodes: installed.iter().map(xcode::Xcode::status).collect(),
        };
        (report, warming)
    }

    fn warm_up(&self) -> MutexGuard<'_, Option<WarmUp>> {
        // Each update is one assignment or one increment.
        self.warm_up.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn xcodes(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<String, PathBuf>> {
        self.xcodes.read().unwrap_or_else(PoisonError::into_inner)
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
        let developer_dir = xcode::developer_dir(&self.xcodes(), &action, &command)?;

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
        // The arguments and the whole environment, the lease's own directories first
        // and the Command's variables winning over them; the child execs these itself.
        let env = (prepared.lease_env.iter())
            .map(|(k, v)| (OsStr::new(k), v.as_os_str()))
            .chain(
                prepared
                    .env
                    .iter()
                    .map(|(k, v)| (OsStr::new(k), OsStr::new(v))),
            )
            .chain(
                (prepared.developer_dir.iter())
                    .map(|d| (OsStr::new(xcode::DEVELOPER_DIR), d.as_os_str())),
            );
        let exec = Exec::new(&program, args.iter().map(OsStr::new), env)
            .map_err(failed(&prepared.program))?;
        let mut command = tokio::process::Command::new(&program);
        command
            .current_dir(&prepared.work_dir)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0)
            .kill_on_drop(true);
        let record = crate::record::path(&self.config.scratch, &lease_name(work.lease_id));
        let (mut child, leader) = spawn(&mut command, exec, &record)
            .await
            .map_err(failed(&prepared.program))?;
        let me = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
        let mut group = Group::new(Tracker::new(leader, me), record);
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
            path: self.config.scratch.join(lease_name(work.lease_id)),
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

/// How [`NativeRuntime::warm_xcrun`] was asked to warm `xcrun`.
#[derive(Debug)]
struct WarmUp {
    xcrun: PathBuf,
    within: Duration,
    /// How many warm-ups were started, which numbers the next one's directory.
    started: u32,
}

/// A warm-up of `xcrun` for `developer_dirs` (`None` the node's own Xcode) in `dir`
/// under the scratch root ([`NativeRuntime::warm_xcrun`]), not generic over the CAS.
fn warm_xcrun(
    config: &NativeConfig,
    rules: &str,
    dir: &str,
    xcrun: &Path,
    developer_dirs: Vec<Option<PathBuf>>,
    within: Duration,
) -> Option<std::thread::JoinHandle<()>> {
    let sandbox = xcode::WarmSandbox {
        isolation: config.isolation.clone(),
        dir: config.scratch.join(dir),
        rules: rules.to_owned(),
    };
    xcode::warm(xcrun, developer_dirs, within, sandbox)
}

/// Removes the leftovers in the user folders old enough to be no lease's work in
/// progress ([`crate::user_folders`]), by descriptor. Not generic over the CAS, like
/// [`sweep_user_folders_after_lease`], so every test binary runs the one copy.
fn sweep_user_folders(config: &NativeConfig) {
    if let Some(folders) = &config.user_folders {
        folders.sweep(
            config.leftover_age,
            SystemTime::now(),
            &kbf_outputs::remove_tree_at,
        );
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
        folders.sweep(age, SystemTime::now(), &kbf_outputs::remove_tree_at);
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

/// The name of a lease's directory, and of its run record.
fn lease_name(id: LeaseId) -> String {
    format!("lease-{}-{}", id.term, id.seq)
}

/// The processes of one running action, and their run record ([`crate::record`]), which
/// goes once they have all ended. Dropped while armed (the daemon dropped the run), it
/// kills them all, blocking.
struct Group {
    tracker: Arc<Mutex<Tracker>>,
    record: PathBuf,
    /// How the process table is read: [`procs::snapshot`], but for tests.
    snapshot: Snapshot,
    armed: bool,
}

/// A way to read the process table.
type Snapshot = fn() -> std::io::Result<Vec<Proc>>;

impl Group {
    fn new(tracker: Tracker, record: PathBuf) -> Self {
        Self::reading(tracker, record, procs::snapshot)
    }

    /// [`Group::new`], reading the process table with `snapshot`.
    fn reading(tracker: Tracker, record: PathBuf, snapshot: Snapshot) -> Self {
        Self {
            tracker: Arc::new(Mutex::new(tracker)),
            record,
            snapshot,
            armed: true,
        }
    }

    /// The live processes of the action now, on the blocking pool.
    async fn members(&self) -> Result<Vec<Proc>, RuntimeError> {
        let (tracker, snapshot) = (Arc::clone(&self.tracker), self.snapshot);
        // A task that did not finish (a panic, a runtime shutting down) is an I/O
        // failure like the call's own.
        let members = tokio::task::spawn_blocking(move || members_now(&tracker, snapshot)).await;
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
    /// none alive, then removes the run record and reaps the leader. Processes still
    /// alive after `wait` fail the lease, and keep their record for the next sweep.
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
        let _ = std::fs::remove_file(&self.record);
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

fn members_now(tracker: &Mutex<Tracker>, snapshot: Snapshot) -> std::io::Result<Vec<Proc>> {
    let snapshot = snapshot()?;
    Ok(lock(tracker).members(&snapshot))
}

impl Drop for Group {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let ended = (0..100).any(|_| {
            let members = members_now(&self.tracker, self.snapshot).unwrap_or_default();
            procs::kill_all(&lock(&self.tracker), &members);
            std::thread::sleep(KILL_PAUSE);
            members.is_empty()
        });
        // Survivors keep their record, for the next start's sweep.
        let _ = ended.then(|| std::fs::remove_file(&self.record));
        tracing::warn!(ended, "run dropped; killed the action's processes");
    }
}

/// Spawns `command` running `exec`, with its run record written to `record` before
/// the program runs ([`crate::record::Gate`]), retrying while the program is busy: a
/// just-written input file is briefly held open for writing by any child another thread
/// forks before it execs (ETXTBSY). Returns the child and its pid.
async fn spawn(
    command: &mut tokio::process::Command,
    exec: Exec,
    record: &Path,
) -> std::io::Result<(Child, i32)> {
    let gate = crate::record::Gate::install(command, exec);
    let give_up = Instant::now() + BUSY_WAIT;
    loop {
        match gate.spawn(command, record) {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < give_up => {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            spawned => return spawned,
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

    /// Catches (issue #164): a survey that leaves the runtime selecting the Xcodes of
    /// the one before (a fixed Xcode refused to the actions placed for it, or a broken
    /// one still run), a not-ready Xcode advertised, the network entry dropped from the
    /// report, and an installed Xcode missing from the status.
    #[test]
    fn a_survey_changes_the_xcodes_actions_may_name() {
        let scratch = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("apply-{}", std::process::id()));
        let _ = kbf_outputs::remove_tree(&scratch);
        let mut config = NativeConfig::new(scratch.clone());
        config.xcodes = BTreeMap::from([("1A".to_owned(), PathBuf::from("/A/Contents/Developer"))]);
        let runtime = NativeRuntime::new(config, Arc::new(NoCas)).expect("runtime");
        let network = (
            network::CAPABILITY.to_owned(),
            runtime.config.isolation.name().to_owned(),
        );
        let xcode_entry = |build: &str| (xcode::CAPABILITY.to_owned(), build.to_owned());
        assert_eq!(runtime.capabilities(), [network.clone(), xcode_entry("1A")]);
        let at = |app: &str, build: &str, state| xcode::Xcode {
            app: PathBuf::from(app),
            developer_dir: Some(PathBuf::from(app).join("Contents/Developer")),
            build: Some(build.to_owned()),
            state,
            reason: String::new(),
        };
        let installed = [
            at("/B", "2B", xcode::State::Ready),
            at("/C", "3C", xcode::State::LicenseNotAccepted),
        ];
        let report = runtime.apply_xcodes(&installed);
        assert_eq!(report.entries, [network, xcode_entry("2B")]);
        assert_eq!(report.entries, runtime.capabilities());
        let states: Vec<_> = report
            .xcodes
            .iter()
            .map(|x| (x.app.as_str(), x.state()))
            .collect();
        assert_eq!(
            states,
            [
                ("/B", kbf_proto::worker::XcodeState::Ready),
                ("/C", kbf_proto::worker::XcodeState::LicenseNotAccepted)
            ]
        );
        let named = |build: &str| Action {
            platform: Some(kbf_proto::reapi::Platform {
                properties: vec![kbf_proto::reapi::platform::Property {
                    name: "xcode".to_owned(),
                    value: build.to_owned(),
                }],
            }),
            ..Action::default()
        };
        let select =
            |build| xcode::developer_dir(&runtime.xcodes(), &named(build), &Command::default());
        assert_eq!(
            select("2B").ok(),
            Some(Some(PathBuf::from("/B/Contents/Developer")))
        );
        assert!(select("1A").is_err(), "the earlier survey's Xcode");
        assert!(select("3C").is_err(), "a not-ready Xcode");
        kbf_outputs::remove_tree(&scratch).expect("clean");
    }

    /// Catches a warm-up the runtime starts outside its own sandbox: not through this
    /// node's isolation program, without the user-folder rules (then `xcrun` cannot
    /// write its cache), in another directory than [`WARM_UP_DIR`] under the scratch
    /// root, or for other Xcodes than the node's own and the configured ones. The
    /// sandbox program is a fake that logs its lease parameter and whether the profile
    /// carries the rules, then runs the rest.
    #[test]
    fn the_runtime_warms_xcrun_under_its_sandbox() {
        use std::os::unix::fs::PermissionsExt as _;

        let scratch = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("warm-{}", std::process::id()));
        let _ = kbf_outputs::remove_tree(&scratch);
        std::fs::create_dir_all(scratch.join("T")).expect("T");
        std::fs::create_dir_all(scratch.join("C")).expect("C");
        let scratch = std::fs::canonicalize(&scratch).expect("real");
        let script = |path: &Path, text: String| {
            std::fs::write(path, text).expect("script");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        };
        let log = scratch.join("log");
        let sandbox_exec = scratch.join("sandbox-exec");
        script(
            &sandbox_exec,
            format!(
                "#!/bin/sh\n\
                 case \"$4\" in *xcrun_db*) rules=rules ;; *) rules=none ;; esac\n\
                 echo \"$2 $rules ${{DEVELOPER_DIR-own}}\" >> '{}'\nshift 4\nexec \"$@\"\n",
                log.display()
            ),
        );
        let xcrun = scratch.join("xcrun");
        script(&xcrun, "#!/bin/sh\nexit 0\n".to_owned());
        let mut config = NativeConfig::new(scratch.join("leases"));
        config.isolation = network::Isolation::Sandbox(sandbox_exec);
        config.user_folders = UserFolders::new(scratch.join("T"), scratch.join("C"));
        config.xcodes = BTreeMap::from([("1A1".to_owned(), PathBuf::from("/x1"))]);
        let rt = NativeRuntime::with_remover(config, Arc::new(NoCas), &kbf_outputs::remove_tree)
            .expect("started");
        rt.warm_xcrun(&xcrun, Duration::from_secs(5))
            .expect("a thread")
            .join()
            .expect("warmed");
        let lease = format!(
            "KBF_LEASE={}",
            scratch.join("leases").join(WARM_UP_DIR).display()
        );
        let want: Vec<String> = ["own", "/x1"]
            .iter()
            .flat_map(|dir| xcode::WARM_TOOLS.map(|_| format!("{lease} rules {dir}")))
            .collect();
        let got = std::fs::read_to_string(&log).expect("log");
        assert_eq!(got.lines().collect::<Vec<_>>(), want);
        kbf_outputs::remove_tree(&scratch).expect("clean");
    }

    /// Catches (the merge of issues #163 and #164): an Xcode a later survey makes ready
    /// never warmed, so the first action to name it pays an uncached `xcrun` lookup
    /// per tool; one that was ready already warmed again on every change; a warm-up
    /// started before the daemon asked for one; one run outside the sandbox or
    /// without the rules; and a later warm-up in the first one's directory (each
    /// removes its own when it ends, so two that overlap would break each other).
    #[test]
    fn each_xcode_a_survey_makes_ready_is_warmed() {
        use std::os::unix::fs::PermissionsExt as _;

        let scratch = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("rewarm-{}", std::process::id()));
        let _ = kbf_outputs::remove_tree(&scratch);
        std::fs::create_dir_all(scratch.join("T")).expect("T");
        std::fs::create_dir_all(scratch.join("C")).expect("C");
        let scratch = std::fs::canonicalize(&scratch).expect("real");
        let script = |path: &Path, text: String| {
            std::fs::write(path, text).expect("script");
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        };
        let log = scratch.join("log");
        let sandbox_exec = scratch.join("sandbox-exec");
        script(
            &sandbox_exec,
            format!(
                "#!/bin/sh\n\
                 case \"$4\" in *xcrun_db*) rules=rules ;; *) rules=none ;; esac\n\
                 echo \"$2 $rules ${{DEVELOPER_DIR-own}}\" >> '{}'\nshift 4\nexec \"$@\"\n",
                log.display()
            ),
        );
        let xcrun = scratch.join("xcrun");
        script(&xcrun, "#!/bin/sh\nexit 0\n".to_owned());
        let mut config = NativeConfig::new(scratch.join("leases"));
        config.isolation = network::Isolation::Sandbox(sandbox_exec);
        config.user_folders = UserFolders::new(scratch.join("T"), scratch.join("C"));
        let rt = NativeRuntime::with_remover(config, Arc::new(NoCas), &kbf_outputs::remove_tree)
            .expect("started");
        let at = |app: &str, state| xcode::Xcode {
            app: PathBuf::from(app),
            developer_dir: Some(PathBuf::from(app).join("Contents/Developer")),
            build: Some(app.trim_start_matches('/').to_owned()),
            state,
            reason: String::new(),
        };
        let (ready, licence) = (xcode::State::Ready, xcode::State::LicenseNotAccepted);
        let read = || -> Vec<String> {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        };
        let lookups = |dir: &str, developer_dir: &str| -> Vec<String> {
            let lease = scratch.join("leases").join(dir);
            xcode::WARM_TOOLS
                .map(|_| format!("KBF_LEASE={} rules {developer_dir}", lease.display()))
                .into()
        };
        // Before the daemon asks for a warm-up, a survey starts none.
        let (_, none) = rt.apply_and_warm(&[at("/A", ready), at("/B", licence)]);
        assert!(none.is_none(), "warmed before warm_xcrun");
        rt.warm_xcrun(&xcrun, Duration::from_secs(5))
            .expect("a thread")
            .join()
            .expect("warmed");
        let mut want = lookups(WARM_UP_DIR, "own");
        want.extend(lookups(WARM_UP_DIR, "/A/Contents/Developer"));
        assert_eq!(read(), want);
        // `B` fixed: only it is warmed, in a directory of its own.
        let (_, fixed) = rt.apply_and_warm(&[at("/A", ready), at("/B", ready)]);
        fixed.expect("a thread").join().expect("warmed");
        want.extend(lookups(
            &format!("{WARM_UP_DIR}-2"),
            "/B/Contents/Developer",
        ));
        assert_eq!(read(), want);
        // Nothing newly ready: nothing warmed.
        let (_, same) = rt.apply_and_warm(&[at("/A", ready), at("/B", licence)]);
        assert!(same.is_none(), "warmed an Xcode that was ready");
        assert_eq!(read(), want);
        for dir in [WARM_UP_DIR.to_owned(), format!("{WARM_UP_DIR}-2")] {
            assert!(!scratch.join("leases").join(&dir).exists(), "{dir} stays");
        }
        kbf_outputs::remove_tree(&scratch).expect("clean");
    }

    /// Catches a start, or the end of a lease, that does not sweep the user folders'
    /// old leftovers, and a runtime whose actions do not get the folders' sandbox rules.
    /// This build's own copy of both sweeps: `tests/leftovers.rs` runs real leases.
    #[tokio::test]
    async fn the_user_folders_are_swept_at_start_and_after_a_lease() {
        let scratch = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("user-folders-{}", std::process::id()));
        let _ = kbf_outputs::remove_tree(&scratch);
        let left = scratch.join("T/TemporaryItems/left");
        let leave = || {
            std::fs::create_dir_all(&left).expect("mkdir");
            std::fs::File::open(&left)
                .expect("open")
                .set_modified(SystemTime::UNIX_EPOCH)
                .expect("mtime");
        };
        leave();
        std::fs::create_dir_all(scratch.join("C")).expect("mkdir");
        let folders = UserFolders::new(scratch.join("T"), scratch.join("C")).expect("fits");
        let mut config = NativeConfig::new(scratch.join("leases"));
        config.user_folders = Some(folders.clone());
        let started =
            NativeRuntime::with_remover(config, Arc::new(NoCas), &kbf_outputs::remove_tree)
                .expect("started");
        assert_eq!(started.rules, folders.rules());
        assert!(!left.exists(), "kept past start");
        leave();
        sweep_user_folders_after_lease(&started.config).await;
        assert!(!left.exists(), "kept past a lease");
        // Without folders (off macOS) there is nothing to sweep.
        leave();
        let mut none = started.config.clone();
        none.user_folders = None;
        sweep_user_folders_after_lease(&none).await;
        assert!(
            left.exists(),
            "swept folders the configuration does not name"
        );
        kbf_outputs::remove_tree(&scratch).expect("clean");
    }

    /// A process table with one process, above any kernel's pid limit, that never dies.
    fn undying() -> std::io::Result<Vec<Proc>> {
        Ok(vec![Proc {
            pid: 2_000_000_005,
            ppid: 1,
            pgid: 2_000_000_005,
            start: 5,
            zombie: false,
        }])
    }

    fn nothing() -> std::io::Result<Vec<Proc>> {
        Ok(Vec::new())
    }

    /// Catches a dropped run that removes its run record although processes of it
    /// survived SIGKILL (the next start could not find them), and one that keeps the
    /// record once they have all ended.
    #[test]
    fn a_dropped_run_keeps_its_record_only_while_processes_survive() {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("group-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let record = dir.join("lease-5-5");
        std::fs::write(&record, "x").expect("write");
        let me = i32::try_from(std::process::id()).expect("pid");
        let tracker = || Tracker::new(2_000_000_005, me);
        drop(Group::reading(tracker(), record.clone(), undying));
        assert!(record.exists(), "kept for the next start's sweep");
        drop(Group::reading(tracker(), record.clone(), nothing));
        assert!(!record.exists(), "removed once nothing survives");
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
        let command = || tokio::process::Command::new(&program);
        let exec = || Exec::new(&program, [], []).expect("exec");
        let record = dir.join(format!("busy-record-{}", std::process::id()));
        let started = Instant::now();
        let (mut child, pid) = spawn(&mut command(), exec(), &record)
            .await
            .expect("spawned once closed");
        assert!(started.elapsed() >= Duration::from_millis(50), "it waited");
        assert!(pid > 0);
        assert_eq!(child.wait().await.expect("wait").code(), Some(7));
        // As the lease's end does: a record is written only where none is.
        std::fs::remove_file(&record).expect("the run record");
        closer.await.expect("closer");
        // Held open for writing past the wait: the spawn gives up with ETXTBSY.
        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(&program)
            .expect("open");
        let busy = spawn(&mut command(), exec(), &record)
            .await
            .expect_err("still busy");
        assert_eq!(busy.raw_os_error(), Some(libc::ETXTBSY));
        drop(held);
        // Any other failure is not retried: a file that is no program at all.
        std::fs::write(&program, [0x7f, b'E', b'L', b'F', 0, 0]).expect("write");
        let started = Instant::now();
        let error = spawn(&mut command(), exec(), &record)
            .await
            .expect_err("not a program");
        assert_ne!(error.raw_os_error(), Some(libc::ETXTBSY));
        assert!(started.elapsed() < BUSY_WAIT);
        assert!(!record.exists(), "a failed spawn leaves no run record");
        std::fs::remove_file(&program).expect("remove");
    }
}
