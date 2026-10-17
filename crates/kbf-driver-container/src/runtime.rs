//! [`ContainerRuntime`]: one action, one fresh rootless `crun` container, never reused.
//!
//! Each lease goes through the six driver steps
//! (`docs/design/daemon.md#the-container-driver`):
//! 1. **prepare:** fetch the Action and Command, check the image and paths, write the
//!    input root into the lease's scratch directory, move each input an output names
//!    into the overlay's upper directory, give the overlay's directories to the
//!    container's root (a subordinate id, see `oci::helper`), make the lease cgroup;
//! 2. **start:** write the bundle, then `crun run` on a thread of its own;
//! 3. **watch:** wait for the exit, the timeout, [`Runtime::kill`], or a lease that
//!    keeps pressing its own memory cap into swap ([`SwapKill`]), which the driver
//!    kills and reports as out of memory;
//! 4. **collect:** the exit code from `crun run`'s status (a container crun could not
//!    start because the program is not there or not executable is the action's exit
//!    127 or 126, see `program`), an OOM kill and whose limit caused it from the lease
//!    cgroup's `memory.events`; the overlay's directories back to the daemon's user;
//!    outputs, stdout and stderr into the CAS;
//! 5. **clean:** remove the container, the lease cgroup and the scratch directory;
//! 6. **verify-clean:** neither directory may remain.
//!
//! Cleaning runs on every path out of `run`: success, failure, timeout and kill. If the
//! daemon drops the `run` future instead (its task is cancelled), the lease's `Drop`
//! cleans up, blocking, before the future is gone. Either way the clean first kills the
//! lease cgroup and makes sure `crun run` has exited, so nothing the driver started is
//! left running while it removes the lease. A clean that fails turns the lease into a
//! failure: a dirty node must be loud. The clean in `Drop` blocks the thread that drops
//! the run, usually a runtime worker: at worst about five seconds for `crun run` to end,
//! five more for the lease cgroup to empty, then `crun delete --force` (and a helper
//! container for files the container's ids still own), which nothing here bounds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use kbf_daemon::cache::{Cache, CacheConfig, InputStats};
use kbf_daemon::cas::{Cas, CasError};
use kbf_daemon::cpu_pool::{CpuPool, DEFAULT_CPU_THREADS};
use kbf_daemon::trees::{TreeConfig, TreeLease, Trees};
use kbf_daemon::{Runtime, RuntimeError, Work, upload_within};
use kbf_proto::reapi::{Action, ActionResult, Command, ExecutedActionMetadata, Platform};
use kbf_proto::worker::ResourceUsage;
use kbf_types::LeaseId;
use prost_types::Timestamp;
use tokio::sync::oneshot;

use crate::cgroup::{LeaseCgroup, SwapKill, memory_max, presses_into_swap};
use crate::cgroup_usage::{NUMA_EVERY, NumaSample, resource_usage};
use crate::container::{
    CONTAINER_OWNER, ContainerLimits, ContainerSpec, DAEMON_OWNER, Dirs, Ended, LEASE_PREFIX,
};
use crate::image::{ImageCheckError, ImageRef, PROPERTY};
use crate::input_phases::{InputPhases, millis};
use crate::keeper::Keeper;
use crate::numa::{Pin, Placement, Placer};
use crate::oci::{Oci, OciConfig};
use crate::outputs::{OutputLimits, collect_log};
use crate::program;
use crate::remove::remove_tree;
use crate::running::Running;
use crate::tree::{
    TreeError, check_relative, collect, fetch_message, materialize_cached, materialize_phased,
    output_paths, prepare_outputs, refuse_hidden_working_directory,
};

/// The driver name in the node report.
pub const DRIVER: &str = "container";
/// The lease kind this driver runs.
pub const KIND: &str = "action";

/// Where and how the driver runs containers.
#[derive(Clone, Debug)]
pub struct ContainerConfig {
    /// `crun` and the id files the containers' ids are read from (`--oci-runtime`).
    pub oci: OciConfig,
    /// The directory each lease's scratch directory is made in.
    pub scratch: PathBuf,
    /// Where the cgroup v2 filesystem is mounted.
    pub cgroup_root: PathBuf,
    /// The daemon's delegated `actions/` cgroup, relative to `cgroup_root` and starting
    /// with `/`. Its `cgroup.subtree_control` must enable `cpu`, `memory` and `pids`.
    pub cgroup_parent: String,
    /// The timeout of an action that names none.
    pub default_timeout: Duration,
    /// How long a container gets after SIGTERM before `cgroup.kill`.
    pub kill_grace: Duration,
    /// How much output one action may leave; past it, the action fails.
    pub outputs: OutputLimits,
    /// How long one lease's whole output upload (outputs, stdout and stderr) may take
    /// before the lease fails saying so ([`kbf_daemon::upload_within`]).
    pub upload_deadline: Duration,
    /// The pids, `/dev/shm` and ulimits each container is created with.
    pub limits: ContainerLimits,
    /// When a lease at its own memory cap that keeps pushing into swap is killed.
    pub swap_kill: SwapKill,
    /// How often the lease cgroup's `memory.numa_stat` is read while the action runs
    /// (measurement only: nothing decides on it). Tests make it short.
    pub numa_every: Duration,
    /// The node's input cache, if it has one (`<--data-dir>/cache`). Without it, a lease's
    /// input files are its own copies, as before the cache.
    pub cache: Option<CacheUse>,
    /// The threads that hash an action's output files (`--cpu-threads`), shared by every
    /// lease.
    pub cpu: CpuPool,
    /// Places each lease on one NUMA node (`--lease-placement=numa-node`); `None` leaves
    /// them on any CPU the daemon may use.
    pub placer: Option<Arc<Placer>>,
}

/// The node's input cache as the driver is given it: not opened yet.
/// [`ContainerRuntime::new`] opens it, and runs uncached, after a warning, if it cannot.
#[derive(Clone)]
pub struct CacheUse {
    pub config: CacheConfig,
    /// The `kbf-daemon` binary, run in a helper container in its `__chown-dirs` mode
    /// ([`crate::chown_dirs`]) to give the input root's directories, not its files, to
    /// the container's root and back.
    pub helper: PathBuf,
    /// The node's shared read-only trees (`<--data-dir>/trees`), if it has them: large
    /// Directory nodes of an input root are built once and mounted read-only, not
    /// linked file by file. Opened with the cache; needs it.
    pub trees: Option<TreeConfig>,
}

impl std::fmt::Debug for CacheUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheUse")
            .field("dir", &self.config.dir)
            .field("helper", &self.helper)
            .finish_non_exhaustive()
    }
}

/// The node's input cache, opened.
#[derive(Debug)]
struct Cached {
    cache: Arc<Cache>,
    helper: PathBuf,
    /// The shared trees, if the node has a store.
    trees: Option<Trees>,
}

impl ContainerConfig {
    /// A configuration for containers run by `crun`, cgroup v2 at
    /// `/sys/fs/cgroup`, a one hour default timeout, a five second kill grace,
    /// the default [`OutputLimits`], [`kbf_daemon::UPLOAD_DEADLINE`],
    /// [`ContainerLimits::DEFAULT`] and
    /// [`SwapKill::DEFAULT`], and no NUMA placement.
    #[must_use]
    pub fn new(scratch: PathBuf, cgroup_parent: String) -> Self {
        Self {
            oci: OciConfig::default(),
            scratch,
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            cgroup_parent,
            default_timeout: Duration::from_secs(3600),
            kill_grace: Duration::from_secs(5),
            outputs: OutputLimits::DEFAULT,
            upload_deadline: kbf_daemon::UPLOAD_DEADLINE,
            limits: ContainerLimits::DEFAULT,
            swap_kill: SwapKill::DEFAULT,
            numa_every: NUMA_EVERY,
            cache: None,
            cpu: CpuPool::new(DEFAULT_CPU_THREADS),
            placer: None,
        }
    }

    /// Whether this configuration can be used.
    fn check(&self) -> Result<(), ConfigError> {
        let scratch = self.scratch.to_string_lossy();
        if !self.scratch.is_absolute() || scratch.contains([':', ',']) {
            return Err(ConfigError::Scratch(self.scratch.clone()));
        }
        if !self.cgroup_parent.starts_with('/') {
            return Err(ConfigError::CgroupParent(self.cgroup_parent.clone()));
        }
        let limits = self.limits;
        for (name, value) in [
            ("pids", limits.pids),
            ("shm", limits.shm_mib),
            ("nofile", limits.nofile),
            ("nproc", limits.nproc),
        ] {
            // A pids limit of 0 would leave nothing to run with, and so would a 0 for
            // the others.
            if value == 0 {
                return Err(ConfigError::Limit(name));
            }
        }
        Ok(())
    }
}

/// Why a [`ContainerConfig`] cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// An overlay mount's options separate fields with `,` and lower directories with `:`.
    #[error("scratch directory {0:?} must be absolute and contain no ':' or ','")]
    Scratch(PathBuf),
    #[error("cgroup parent {0:?} must start with '/'")]
    CgroupParent(String),
    /// One of the [`ContainerLimits`], by name, is 0.
    #[error("the container {0} limit must be at least 1")]
    Limit(&'static str),
}

/// Why a [`ContainerRuntime`] does not start.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// What a previous daemon left could not be listed or removed. Starting anyway
    /// could run a lease's work beside its leftover container.
    #[error("removing what a previous daemon left: {0}")]
    Sweep(String),
    /// `crun` cannot run containers on this node.
    #[error("the container runtime: {0}")]
    Oci(String),
}

/// How long the clean waits for a `crun run` to end once its cgroup is killed, and how
/// often it looks. A killed process exits once it leaves the system call it is in.
const REAP_LIMIT: Duration = Duration::from_secs(5);
const REAP_PAUSE: Duration = Duration::from_millis(2);

/// A killer waiting for a lease's work to stop.
type Stop = oneshot::Sender<()>;

/// A prepared lease: what the bundle needs and what collecting needs.
struct Prepared {
    spec: ContainerSpec,
    outputs: Vec<String>,
    timeout: Duration,
}

/// Runs `action` leases in rootless `crun` containers.
#[derive(Debug)]
pub struct ContainerRuntime<C> {
    config: ContainerConfig,
    engine: Arc<Oci>,
    cas: Arc<C>,
    stops: Mutex<BTreeMap<LeaseId, oneshot::Sender<Stop>>>,
    /// What leases that ended without a result counted, until the daemon takes it
    /// ([`Runtime::failed_usage`]).
    failed: Mutex<BTreeMap<LeaseId, ResourceUsage>>,
    /// Samples of `memory.numa_stat` that held memory, over every lease.
    numa_samples: Arc<AtomicU64>,
    /// The input cache leases link from, if the node has one and it could be used.
    cache: Option<Cached>,
    /// What the last lease's inputs cost, with the cache (a test's counter; each
    /// lease's are logged).
    last_inputs: Mutex<Option<InputStats>>,
}

impl<C: Cas> ContainerRuntime<C> {
    /// A runtime that reads and writes blobs through `cas`. Before it returns, so
    /// before the daemon says `Hello`, it removes what a previous daemon left: every
    /// lease scratch directory (killing what still runs in it), each with its container
    /// and lease cgroup. Blocking.
    ///
    /// # Errors
    /// The configuration is unusable, `crun` cannot run containers on this node, or a
    /// leftover could not be removed. When the cache cannot be opened it warns and runs
    /// uncached.
    pub fn new(config: ContainerConfig, cas: Arc<C>) -> Result<Self, StartError> {
        config.check()?;
        let engine = Oci::new(&config.oci, &config.scratch, &config.cgroup_parent)
            .map_err(|e| StartError::Oci(e.to_string()))?;
        let mut runtime = Self {
            engine,
            config,
            cas,
            stops: Mutex::new(BTreeMap::new()),
            failed: Mutex::new(BTreeMap::new()),
            numa_samples: Arc::new(AtomicU64::new(0)),
            cache: None,
            last_inputs: Mutex::new(None),
        };
        sweep(&runtime.engine, &runtime.config).map_err(StartError::Sweep)?;
        if let Some(given) = runtime.config.cache.clone() {
            runtime.cache = open_cache(given);
        }
        Ok(runtime)
    }

    /// Whether leases link their inputs from the cache.
    #[must_use]
    pub fn cached(&self) -> bool {
        self.cache.is_some()
    }

    /// What the node's tree store has done, if it has one.
    #[must_use]
    pub fn tree_stats(&self) -> Option<kbf_daemon::trees::TreeStats> {
        let cached = self.cache.as_ref();
        cached
            .and_then(|c| c.trees.as_ref())
            .map(|t| t.store().stats())
    }

    /// What the last lease's inputs cost through the cache, if a lease used it.
    #[must_use]
    pub fn last_inputs(&self) -> Option<InputStats> {
        *self
            .last_inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// How many readings of a lease cgroup's `memory.numa_stat`, taken while an action
    /// ran or when it ended, held memory (a test waits on it to know the sampler ran).
    #[must_use]
    pub fn numa_samples(&self) -> u64 {
        self.numa_samples.load(Ordering::Acquire)
    }

    fn failed(&self) -> MutexGuard<'_, BTreeMap<LeaseId, ResourceUsage>> {
        // Each update is one map operation, so the map is consistent after a panic.
        self.failed.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stops(&self) -> MutexGuard<'_, BTreeMap<LeaseId, oneshot::Sender<Stop>>> {
        // Each update is one map operation, so the map is consistent after a panic.
        self.stops.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Step 1: everything up to writing the bundle, through the lease's `cas`.
    async fn prepare(
        &self,
        cas: &C::Lease,
        work: &Work,
        lease: &mut Lease,
    ) -> Result<Prepared, RuntimeError> {
        let (mut phases, started) = (InputPhases::default(), Instant::now());
        let action: Action = fetch_message(cas, &work.action_digest)
            .await
            .map_err(tree_error)?;
        let command_digest = action
            .command_digest
            .as_ref()
            .ok_or_else(|| RuntimeError::Invalid("the Action names no Command".to_owned()))?;
        let command: Command = fetch_message(cas, command_digest)
            .await
            .map_err(tree_error)?;
        phases.action_ms = millis(started);
        let input_root = action
            .input_root_digest
            .as_ref()
            .ok_or_else(|| RuntimeError::Invalid("the Action names no input root".to_owned()))?;
        let image = image_of(&action, &command)?;
        if command.arguments.is_empty() {
            return Err(RuntimeError::Invalid(
                "the Command has no arguments".to_owned(),
            ));
        }
        check_relative("working directory", &command.working_directory).map_err(tree_error)?;
        let outputs = output_paths(&command).map_err(tree_error)?;
        let timeout = timeout_of(&action, self.config.default_timeout)?;

        let started = Instant::now();
        self.check_image(&image).await?;
        phases.image_ms = millis(started);

        let root = lease.dir.join("root");
        let upper = lease.dir.join("upper");
        let overlay_work = lease.dir.join("work");
        for dir in [&lease.dir, &root, &upper, &overlay_work] {
            tokio::fs::create_dir(dir)
                .await
                .map_err(|e| failed(dir, &e))?;
        }
        let mut shared = Vec::new();
        match &self.cache {
            Some(cache) => {
                let private = private_paths(&command.working_directory, &outputs);
                let stats = match &cache.trees {
                    Some(trees) => {
                        let inputs = trees
                            .materialize(
                                cas,
                                &cache.cache,
                                input_root,
                                &root,
                                &private,
                                &command.working_directory,
                            )
                            .await
                            .map_err(|e| tree_error(e.into()))?;
                        let shown = format!(
                            "{} trees={} tree_files={} trees_built={}",
                            inputs.stats,
                            inputs.mounts.len(),
                            inputs.tree_files,
                            inputs.trees_built
                        );
                        tracing::info!(lease = %lease.name, "inputs: {shown}");
                        (phases.write, phases.trees_built) = (inputs.phases, inputs.trees_built);
                        shared = inputs.mounts;
                        lease.trees = inputs.trees;
                        inputs.stats
                    }
                    None => {
                        let (stats, write) =
                            materialize_cached(cas, &cache.cache, input_root, &root, &private)
                                .await
                                .map_err(tree_error)?;
                        tracing::info!(lease = %lease.name, "inputs: {stats}");
                        phases.write = write;
                        stats
                    }
                };
                *self
                    .last_inputs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(stats);
            }
            None => {
                phases.write = materialize_phased(cas, input_root, &root)
                    .await
                    .map_err(tree_error)?;
            }
        }
        let started = Instant::now();
        // The driver makes the working directory in the upper layer, which would hide
        // an input symlink or file of that name: refused rather than run elsewhere.
        refuse_hidden_working_directory(&root, &command.working_directory)
            .await
            .map_err(tree_error)?;
        // Outputs are read from the upper layer: an input an output names moves there
        // whole, and the working directory and each output's parent are made there.
        prepare_outputs(&root, &upper, &command.working_directory, &outputs)
            .await
            .map_err(tree_error)?;
        phases.prep_ms = millis(started);
        let started = Instant::now();
        // The container's root is a subordinate id of the daemon's user, which may write
        // only under directories it owns.
        self.give_overlay(CONTAINER_OWNER, &lease.dir, &root, &upper, &overlay_work)
            .await?;
        phases.chown_ms = millis(started);
        let started = Instant::now();
        // `actions/`'s own OOM count before the lease, to say afterwards whether its
        // limit killed the lease.
        lease.backstop_ooms = lease.cgroup.parent_ooms().ok();
        // The keeper, before anything runs in the cgroup: it kills the cgroup if the
        // daemon dies.
        let kill = lease.cgroup.dir().join("cgroup.kill");
        let placement = (self.config.placer.as_ref()).map(|placer| {
            placer.place_lease(work.resources.cpu_millis, work.resources.memory_bytes)
        });
        phases.numa_node = placement.as_ref().map(Placement::spot);
        lease.pin = placement.and_then(Placement::into_pin);
        let keeper = (lease.cgroup.create(work.resources, lease.pin.as_ref()))
            .and_then(|()| Keeper::watch(&kill))
            .map_err(|e| failed(lease.cgroup.dir(), &e))?;
        lease.keeper = Some(keeper);
        phases.cgroup_ms = millis(started);
        tracing::info!(lease = %lease.name, "inputs phases: {phases}");

        let spec = ContainerSpec {
            name: lease.name.clone(),
            image: image.to_string(),
            image_digest: image.digest().to_owned(),
            dir: lease.dir.clone(),
            cgroup_parent: lease.cgroup.name().to_owned(),
            input_root: root,
            upper,
            overlay_work,
            working_directory: command.working_directory.clone(),
            env: command
                .environment_variables
                .iter()
                .map(|v| (v.name.clone(), v.value.clone()))
                .collect(),
            argv: command.arguments.clone(),
            limits: self.config.limits,
            shared,
        };
        Ok(Prepared {
            spec,
            outputs,
            timeout,
        })
    }

    /// Steps 1 to 4. `killer` receives the sender of a kill that stopped the work.
    async fn attempt(
        &self,
        work: &Work,
        lease: &mut Lease,
        stop: &mut oneshot::Receiver<Stop>,
        killer: &mut Option<Stop>,
    ) -> Result<ActionResult, RuntimeError> {
        let mut clock = ExecutedActionMetadata {
            worker_start_timestamp: now(),
            input_fetch_start_timestamp: now(),
            ..ExecutedActionMetadata::default()
        };
        // The lease's own view of the CAS, for its transfer counters.
        let cas = &self.cas.lease(&work.lease_id);
        // A kill during prepare (a long input fetch) stops it at once: dropping the
        // future stops it, and the clean step removes whatever was written.
        let Prepared {
            spec,
            outputs,
            timeout,
        } = tokio::select! {
            prepared = self.prepare(cas, work, lease) => prepared?,
            Ok(by) = &mut *stop => {
                *killer = Some(by);
                return Err(RuntimeError::Killed);
            }
        };
        clock.input_fetch_completed_timestamp = now();
        // Execution counts from writing the bundle: making the container is part of
        // running the command in one.
        clock.execution_start_timestamp = now();
        // What the lease cgroup's NUMA split is while the action runs, and the usage
        // read at its end (`cgroup_usage`): `run` reads it too when the lease fails.
        lease.ran = Some(Instant::now());
        lease.numa = NumaSample::counting(Arc::clone(&self.numa_samples));
        lease.created = true;
        self.engine
            .create(&spec)
            .await
            .map_err(RuntimeError::Failed)?;
        let stdout_path = lease.dir.join("stdout");
        let stderr_path = lease.dir.join("stderr");
        let cap = memory_max(work.resources.memory_bytes);
        let swap_kill = self.config.swap_kill;
        let threshold = swap_kill.threshold(work.resources.memory_bytes);
        let mut started = 0;
        let ended = loop {
            started += 1;
            let stdout =
                std::fs::File::create(&stdout_path).map_err(|e| failed(&stdout_path, &e))?;
            let stderr =
                std::fs::File::create(&stderr_path).map_err(|e| failed(&stderr_path, &e))?;
            let start = self
                .engine
                .start(&spec, stdout, stderr)
                .map_err(RuntimeError::Failed)?;
            // The lease owns the `crun run` from here, so a dropped run's clean reaps it.
            let child = lease.start.insert(start);

            tokio::select! {
                // `crun run`'s status is the container's. An error waiting for it is not
                // trusted either way, since `ended` fails the lease unless the run said
                // the container exited, or crun reports it could not start the program
                // (`program::not_run`).
                () = child.wait() => {}
                () = tokio::time::sleep(timeout) => {
                    self.stop_container(&lease.name, &lease.cgroup, child).await;
                    return Err(self.timed_out(cas, &lease.name, &stdout_path, &stderr_path).await);
                }
                Ok(by) = &mut *stop => {
                    *killer = Some(by);
                    self.stop_container(&lease.name, &lease.cgroup, child).await;
                    return Err(RuntimeError::Killed);
                }
                // Never ends: it samples while the others are waited for.
                () = lease.numa.watch(lease.cgroup.dir(), self.config.numa_every) => {}
                (now, own) = presses_into_swap(&lease.cgroup, cap, threshold, swap_kill.every) => {
                    let limit = cap.unwrap_or(0);
                    let why = format!(
                        "at its memory cap of {limit} bytes and still pushing into swap: \
                         {own} of its {} bytes in swap pushed there at its cap (more than \
                         {threshold}); killing it",
                        now.swap
                    );
                    tracing::warn!(lease = %lease.name, "{why}");
                    self.kill_container(&lease.name, &lease.cgroup, child).await;
                    return Err(RuntimeError::OutOfMemory {
                        used: now.current.saturating_add(now.swap),
                        limit,
                    });
                }
            }

            let ended = self.engine.ended(&spec, child).await;
            // An exec that failed with ETXTBSY (`program::text_file_busy`) is started once
            // more: the write descriptor that caused it is gone by now.
            if started == 1
                && matches!(ended, Ok(Ended::NotRun(_)))
                && program::text_file_busy(&self.engine.said(&spec, &stderr_path)).await
            {
                tracing::warn!(lease = %lease.name, "exec found the program busy (ETXTBSY); again");
                continue;
            }
            break ended;
        };
        // SIGKILL (137): whether the kernel's OOM killer did it, and for whose limit,
        // is read from the lease cgroup, not from `crun`. The same is read when `crun`
        // reported no exit or could not be asked: the lease cgroup's counters do not
        // depend on `crun`'s report, so a kill they show is reported as one.
        if matches!(ended, Ok(Ended::Exited(137) | Ended::NotRun(_)) | Err(_)) {
            match lease.cgroup.oom_outcome(cap, lease.backstop_ooms) {
                Ok(Some(kill)) => return Err(kill),
                Ok(None) => {}
                Err(e) if ended == Ok(Ended::Exited(137)) => {
                    return Err(failed(lease.cgroup.dir(), &e));
                }
                // No exit code either: what `crun` said is the one to report.
                Err(_) => {}
            }
        }
        let exit_code = match ended.map_err(RuntimeError::Failed)? {
            Ended::Exited(code) => code,
            Ended::NotRun(state) => {
                program::not_run(&spec, &self.engine.said(&spec, &stderr_path), &state).await?
            }
        };
        clock.execution_completed_timestamp = now();
        // What the lease cgroup counted, while it still exists (clean removes it). The
        // result carries it beside the four numbers every runtime reports.
        let usage = lease.usage();
        clock.auxiliary_metadata = usage.iter().map(ResourceUsage::to_any).collect();
        // The upload counts from the chown, which makes the outputs readable to it.
        clock.output_upload_start_timestamp = now();
        // The container has exited: hand its files back to the daemon's user, so an
        // output the action left unreadable to others is still read, as its owner.
        self.give_overlay(
            DAEMON_OWNER,
            &spec.dir,
            &spec.input_root,
            &spec.upper,
            &spec.overlay_work,
        )
        .await?;
        let mut result = ActionResult {
            exit_code,
            ..ActionResult::default()
        };
        let upload = async {
            // Each output file is hashed once, then sent from where it is as whole
            // segments.
            collect(
                cas,
                &spec.upper,
                &self.config.cpu,
                &spec.working_directory,
                &outputs,
                self.config.outputs,
                &mut result,
            )
            .await
            .map_err(outputs_error)?;
            let max = self.config.outputs.max_stdio_bytes;
            for (path, slot) in [
                (&stdout_path, &mut result.stdout_digest),
                (&stderr_path, &mut result.stderr_digest),
            ] {
                *slot = Some(collect_log(cas, path, max).await.map_err(outputs_error)?);
            }
            Ok(())
        };
        upload_within(self.config.upload_deadline, upload).await?;
        clock.output_upload_completed_timestamp = now();
        clock.worker_completed_timestamp = now();
        result.execution_metadata = Some(clock);
        Ok(result)
    }

    /// Gives the overlay's directories to `owner` (`Oci::give`). Without the cache,
    /// everything under them. With it, only the input root's directories: its files are
    /// links to the cache's entries, which must stay the daemon's (unmapped in the
    /// container, so the action cannot write them); the upper and work directories hold
    /// only the lease's own files.
    async fn give_overlay(
        &self,
        owner: &str,
        lease: &Path,
        root: &Path,
        upper: &Path,
        work: &Path,
    ) -> Result<(), RuntimeError> {
        let dirs = Dirs {
            lease,
            root,
            upper,
            work,
            helper: self.cache.as_ref().map(|cache| cache.helper.as_path()),
        };
        self.engine
            .give(owner, &dirs)
            .await
            .map_err(RuntimeError::Failed)
    }

    /// Checks that this node can run `image`, as [`ContainerRuntime::check_pinned`] does,
    /// and tells a refused image from a failure.
    async fn check_image(&self, image: &ImageRef) -> Result<(), RuntimeError> {
        self.check_pinned(image).await.map_err(|e| match e {
            ImageCheckError::Index(_) => RuntimeError::Invalid(e.to_string()),
            ImageCheckError::Failed(_) => RuntimeError::Failed(e.to_string()),
        })
    }

    /// Checks, as each lease's prepare step does, that this node can run `image` and that
    /// its digest names one architecture's manifest, not an index: that it is unpacked,
    /// which fetches and unpacks it the first time. The daemon checks each image it is
    /// pinned to (`--image`) once at start with it.
    pub async fn check_pinned(&self, image: &ImageRef) -> Result<(), ImageCheckError> {
        self.engine.check_pinned(image).await
    }

    /// [`RuntimeError::TimedOut`] with what the stopped action wrote to stdout and
    /// stderr, each stored within the stdio limit (REAPI's partial result). A log past
    /// the limit, or one that cannot be stored, is left out: the answer is still the
    /// timeout.
    async fn timed_out(
        &self,
        cas: &C::Lease,
        name: &str,
        stdout: &Path,
        stderr: &Path,
    ) -> RuntimeError {
        let max = self.config.outputs.max_stdio_bytes;
        let mut logs = [None, None];
        for (path, slot) in [stdout, stderr].into_iter().zip(&mut logs) {
            match collect_log(cas, path, max).await {
                Ok(digest) => *slot = Some(digest),
                Err(e) => tracing::warn!(lease = %name, "timed out; its log is left out: {e}"),
            }
        }
        let [stdout, stderr] = logs;
        RuntimeError::TimedOut { stdout, stderr }
    }

    /// The swap watch's kill: `cgroup.kill` at once (the lease is over its cap, so no
    /// grace), then the grace period for `crun` to notice. Returns once `crun run`
    /// has ended.
    async fn kill_container(&self, name: &str, cgroup: &LeaseCgroup, child: &mut Running) {
        if let Err(e) = cgroup.kill() {
            tracing::warn!(lease = %name, "cgroup.kill: {e}");
        }
        if tokio::time::timeout(self.config.kill_grace, child.wait())
            .await
            .is_err()
        {
            // The container is removed by force in the clean step either way.
            child.force().await;
        }
    }

    /// The kill path: SIGTERM, the grace period, `cgroup.kill`, then the
    /// grace period again for `crun` to notice. Returns once `crun run` has ended.
    async fn stop_container(&self, name: &str, cgroup: &LeaseCgroup, child: &mut Running) {
        let grace = self.config.kill_grace;
        if let Err(e) = self.engine.terminate(name).await {
            tracing::warn!(lease = %name, "SIGTERM: {e}");
        }
        if tokio::time::timeout(grace, child.wait()).await.is_ok() {
            return;
        }
        tracing::warn!(lease = %name, "still running after SIGTERM; killing its cgroup");
        if let Err(e) = cgroup.kill() {
            tracing::warn!(lease = %name, "cgroup.kill: {e}");
        }
        if tokio::time::timeout(grace, child.wait()).await.is_err() {
            // The container is removed by force in the clean step either way.
            child.force().await;
        }
    }
}

impl<C: Cas> Runtime for ContainerRuntime<C> {
    fn driver(&self) -> &'static str {
        DRIVER
    }

    fn serves(&self, kind: &str) -> bool {
        kind == KIND
    }

    async fn run(&self, work: Work) -> Result<ActionResult, RuntimeError> {
        let (stop_tx, mut stop) = oneshot::channel();
        let _registered = Registered::new(self, work.lease_id, stop_tx);
        let mut lease = Lease::new(&self.engine, &self.config, work.lease_id);
        let mut killer = None;
        let outcome = self
            .attempt(&work, &mut lease, &mut stop, &mut killer)
            .await;
        // A lease that ended without a result (timed out, killed, out of memory) is
        // measured too, before the clean removes its cgroup; the daemon asks for it
        // (`Runtime::failed_usage`).
        let measured = if outcome.is_err() {
            lease.usage()
        } else {
            None
        };
        let cleaned = lease.clean().await;
        // A kill that arrived after the work ended still waits for the clean.
        if let Some(by) = killer.or_else(|| stop.try_recv().ok()) {
            let _ = by.send(());
        }
        let measured = match (&outcome, &cleaned) {
            // A clean that fails turns a result into an error: its usage is still told.
            (Ok(result), Err(_)) => ResourceUsage::in_result(result),
            _ => measured,
        };
        if let Some(usage) = measured {
            self.failed().insert(work.lease_id, usage);
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

    fn failed_usage(&self, lease_id: LeaseId) -> Option<ResourceUsage> {
        self.failed().remove(&lease_id)
    }

    async fn kill(&self, lease_id: LeaseId) {
        let Some(stop) = self.stops().remove(&lease_id) else {
            return;
        };
        let (tx, rx) = oneshot::channel();
        // Answered once the work has stopped and the lease is clean. If the run ended
        // (and cleaned) without seeing the kill, `tx` is dropped with the failed send
        // or with the run's receiver, and `rx` returns at once.
        let _ = stop.send(tx);
        let _ = rx.await;
    }
}

/// A lease's entry in the kill table, removed when its run ends or is dropped.
struct Registered<'a, C> {
    runtime: &'a ContainerRuntime<C>,
    id: LeaseId,
}

impl<'a, C: Cas> Registered<'a, C> {
    fn new(runtime: &'a ContainerRuntime<C>, id: LeaseId, stop: oneshot::Sender<Stop>) -> Self {
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

/// Removes every lease a previous daemon left: each lease scratch directory, removed as
/// a lease's clean removes it (`cgroup.kill`, `crun delete --force`, the lease cgroup,
/// the scratch directory). A lease's `crun` state is inside its scratch directory, so
/// the directories are the whole list. Blocking.
fn sweep(engine: &Arc<Oci>, config: &ContainerConfig) -> Result<(), String> {
    // The scratch directory exists: `Oci::new` made it.
    let entries = std::fs::read_dir(&config.scratch)
        .map_err(|e| format!("{}: {e}", config.scratch.display()))?;
    let names: BTreeSet<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(LEASE_PREFIX))
        .collect();
    let mut errors = Vec::new();
    for name in names {
        tracing::warn!(lease = %name, "removing a lease a previous daemon left");
        let mut lease = Lease::named(engine, config, name);
        lease.created = true;
        if let Err(why) = lease.clean_blocking() {
            errors.push(why);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// What one lease leaves on the node, and how to remove it.
#[derive(Debug)]
struct Lease {
    engine: Arc<Oci>,
    /// `kbf-lease-<term>-<seq>`: the container's name and the lease cgroup's.
    name: String,
    /// The scratch directory.
    dir: PathBuf,
    cgroup: LeaseCgroup,
    /// `actions/`'s own OOM count when the lease cgroup was made, if it could be read.
    backstop_ooms: Option<u64>,
    /// Whether a container may exist.
    created: bool,
    /// The `crun run`, once started. Reaped before anything is removed.
    start: Option<Running>,
    /// Kills the lease cgroup if the daemon dies, once the cgroup is made. Removed
    /// last, once the lease is cleaned.
    keeper: Option<Keeper>,
    /// The shared trees the container mounts, pinned until the lease is gone.
    trees: Vec<TreeLease>,
    /// The NUMA node the lease is placed on, with `--lease-placement=numa-node`. Dropped
    /// with the lease, which gives the node its load back.
    pin: Option<Pin>,
    /// Whether cleaning is still owed.
    armed: bool,
    /// When the container was about to be created, once the lease got that far.
    ran: Option<Instant>,
    /// The NUMA split sampled while the action ran.
    numa: NumaSample,
}

impl Lease {
    fn new(engine: &Arc<Oci>, config: &ContainerConfig, id: LeaseId) -> Self {
        Self::named(
            engine,
            config,
            format!("{LEASE_PREFIX}{}-{}", id.term, id.seq),
        )
    }

    /// The lease named `name` (`kbf-lease-<term>-<seq>`), nothing of it created yet.
    fn named(engine: &Arc<Oci>, config: &ContainerConfig, name: String) -> Self {
        Self {
            engine: engine.clone(),
            dir: config.scratch.join(&name),
            cgroup: LeaseCgroup::at(&config.cgroup_root, &config.cgroup_parent, &name),
            backstop_ooms: None,
            name,
            created: false,
            start: None,
            keeper: None,
            trees: Vec::new(),
            pin: None,
            armed: true,
            ran: None,
            numa: NumaSample::default(),
        }
    }

    /// What the lease cgroup counted so far, with its best NUMA sample: `None` for a
    /// lease that never got to run its container. Read once (it takes the samples);
    /// the cgroup must still exist.
    fn usage(&mut self) -> Option<ResourceUsage> {
        let ran = self.ran?;
        let cgroup = std::mem::take(&mut self.numa).finish(self.cgroup.dir());
        Some(resource_usage(cgroup, ran.elapsed()))
    }

    /// Steps 5 and 6, off the async threads.
    async fn clean(self) -> Result<(), String> {
        tokio::task::spawn_blocking(move || {
            let mut lease = self;
            lease.clean_blocking()
        })
        .await
        .map_err(clean_task_failed)?
    }

    fn clean_blocking(&mut self) -> Result<(), String> {
        self.armed = false;
        let mut errors = Vec::new();
        // First, `crun run`: a dropped run has not waited for it, and until it has exited
        // it may still start the container's processes or write into the lease. It ends
        // when its container does, which nothing else here stops: the cgroup is killed
        // first.
        if let Some(mut start) = self.start.take() {
            let _ = self.cgroup.kill();
            errors.extend(start.reap(REAP_LIMIT, REAP_PAUSE).err());
        }
        if self.created {
            // Kill whatever runs in the lease first: a run dropped mid-start leaves
            // `crun create` in the container's cgroup, and `crun delete --force` returns
            // without waiting for it, so the cgroup stays busy.
            let _ = self.cgroup.kill();
            if let Err(e) = self.engine.remove(&self.name) {
                errors.push(e);
            }
        }
        if let Err(e) = self.cgroup.remove() {
            errors.push(e.to_string());
        }
        // Not `std::fs::remove_dir_all`: it recurses once per level, and the action
        // decides how deep its scratch directory is.
        match remove_tree(&self.dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // Files still owned by the container's ids (a lease that ended before
            // collect handed them back: a timeout, a kill, a failure), or with no
            // permissions left, can only be removed inside the container's user namespace.
            Err(first) => {
                if let Err(e) = self.engine.remove_dir(&self.dir) {
                    errors.push(format!("remove {}: {first}; {e}", self.dir.display()));
                }
            }
        }
        for left in [&self.dir, self.cgroup.dir()] {
            if std::fs::symlink_metadata(left).is_ok() {
                errors.push(format!("{} still exists after cleaning", left.display()));
            }
        }
        drop(self.keeper.take());
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.armed {
            tracing::warn!(lease = %self.name, "run dropped; cleaning up");
            if let Err(why) = self.clean_blocking() {
                tracing::error!(lease = %self.name, "clean: {why}");
            }
        }
    }
}

/// The input paths that must be the lease's own copies, never links to the cache:
/// each output path, seen from the input root (`working_directory/output`). An input
/// there is moved into the overlay's upper directory, which the container's root
/// owns and may change.
fn private_paths(working_directory: &str, outputs: &[String]) -> Vec<String> {
    let base = working_directory.trim_end_matches('/');
    (outputs.iter())
        .map(|output| {
            if base.is_empty() {
                output.clone()
            } else {
                format!("{base}/{output}")
            }
        })
        .collect()
}

/// The cache `given` opened; `None`, after a warning, when it cannot be opened. Blocking.
fn open_cache(given: CacheUse) -> Option<Cached> {
    let shown = given.config.dir.display().to_string();
    match Cache::open(given.config) {
        Ok(cache) => {
            let stats = cache.stats();
            let (bytes, entries, mode) = (stats.bytes, stats.entries, cache.link_mode());
            tracing::info!("cache: {shown}: {bytes} bytes in {entries} entries, link mode {mode}");
            let helper = given.helper;
            let trees = given.trees.and_then(open_trees);
            Some(Cached {
                cache,
                helper,
                trees,
            })
        }
        Err(why) => {
            let reason = why.reason();
            tracing::warn!("cache: off:{reason} ({shown}: {why}); leases run uncached");
            None
        }
    }
}

/// The tree store `config` names; `None`, after a warning, when it cannot be opened:
/// leases then link every file, as without trees. Blocking.
fn open_trees(config: TreeConfig) -> Option<Trees> {
    let shown = config.dir.display().to_string();
    let (min_files, max_mounts) = (config.min_files, config.max_mounts);
    if shown.contains([':', ',']) {
        tracing::warn!("trees: off ({shown}): ':' or ',' in the path");
        return None;
    }
    match Trees::open(config) {
        Ok(trees) => {
            let held = trees.store().stats();
            let (count, bytes) = (held.trees, held.bytes);
            // One line each: a macro's later lines are not run while its level is off.
            tracing::info!("trees: {shown}: {count} trees, {bytes} bytes");
            tracing::info!("trees: min_files={min_files} max_mounts={max_mounts}");
            Some(trees)
        }
        Err(e) => {
            tracing::warn!("trees: off ({shown}): {e}; leases link every file");
            None
        }
    }
}

/// The wall clock now, as `ExecutedActionMetadata` holds it.
fn now() -> Option<Timestamp> {
    Some(Timestamp::from(SystemTime::now()))
}

/// The action's image, from `container-image` in the Action's platform, or the
/// Command's for clients older than REAPI 2.2.
#[allow(deprecated)]
fn image_of(action: &Action, command: &Command) -> Result<ImageRef, RuntimeError> {
    let platform: Option<&Platform> = action.platform.as_ref().or(command.platform.as_ref());
    let value = platform
        .and_then(|p| p.properties.iter().find(|p| p.name == PROPERTY))
        .map(|p| p.value.as_str())
        .ok_or_else(|| {
            RuntimeError::Invalid(format!("the action names no {PROPERTY} platform property"))
        })?;
    ImageRef::parse(value).map_err(|e| RuntimeError::Invalid(e.to_string()))
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

/// A [`TreeError`] reading the action's inputs, as the lease reports it: a missing
/// blob is the client's to upload, a broken rule is the client's error, and anything
/// else is the farm's.
fn tree_error(error: TreeError) -> RuntimeError {
    match error {
        TreeError::Cas(CasError::Missing(blob)) => RuntimeError::MissingBlob(blob),
        other => outputs_error(other),
    }
}

/// A [`TreeError`] storing the action's outputs, as the lease reports it: a broken
/// rule is the client's error and anything else, a missing blob included (it is not
/// the client's to upload), is the farm's.
fn outputs_error(error: TreeError) -> RuntimeError {
    match error {
        TreeError::Invalid(why) => RuntimeError::Invalid(why),
        other => RuntimeError::Failed(other.to_string()),
    }
}

/// The clean step's error when its blocking task did not finish (it panicked, or the
/// runtime shut down before it ran).
fn clean_task_failed(error: tokio::task::JoinError) -> String {
    format!("clean task: {error}")
}

pub(crate) fn failed(path: &Path, error: &std::io::Error) -> RuntimeError {
    RuntimeError::Failed(format!("{}: {error}", path.display()))
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
