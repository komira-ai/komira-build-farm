//! [`PodmanRuntime`]: one action, one fresh rootless Podman container, never reused.
//!
//! Each lease goes through the six driver steps
//! (`docs/design/daemon.md#the-container-driver`):
//! 1. **prepare:** fetch the Action and Command, check the image and paths, write the
//!    input root into the lease's scratch directory, move each input an output names
//!    into the overlay's upper directory, give the overlay's directories to the
//!    container's root (a subordinate id, see `podman::create_args`), make the lease
//!    cgroup;
//! 2. **start:** `podman create`, then `podman start --attach`;
//! 3. **watch:** wait for the exit, the timeout, [`Runtime::kill`], or a lease that
//!    keeps pressing its own memory cap into swap ([`SwapKill`]), which the driver
//!    kills and reports as out of memory;
//! 4. **collect:** the exit code from Podman's record (a container crun could not
//!    start because the program is not there or not executable is the action's exit
//!    127 or 126, see `program`), an OOM kill and whose limit caused it from the lease
//!    cgroup's `memory.events`; the overlay's directories back to the daemon's user;
//!    outputs, stdout and stderr into the CAS;
//! 5. **clean:** remove the container, the lease cgroup and the scratch directory;
//! 6. **verify-clean:** neither directory may remain.
//!
//! Cleaning runs on every path out of `run`: success, failure, timeout and kill. If the
//! daemon drops the `run` future instead (its task is cancelled), the lease's `Drop`
//! cleans up, blocking, before the future is gone. Either way the clean first makes
//! sure `podman start` has exited and been reaped, so nothing the driver started is
//! left running while it removes the lease. A clean that fails turns the lease into a
//! failure: a dirty node must be loud. The clean in `Drop` blocks the thread that drops
//! the run, usually a runtime worker: at worst about five seconds for `podman start` to
//! be reaped, five more for the lease cgroup to empty, then `podman rm` (and `podman
//! unshare rm` for files the container still owns), which nothing here bounds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use kbf_daemon::cache::{Cache, CacheConfig, Disabled, InputStats};
use kbf_daemon::cas::{Cas, CasError};
use kbf_daemon::{Runtime, RuntimeError, Work, upload_within};
use kbf_proto::reapi::{Action, ActionResult, Command, ExecutedActionMetadata, Platform};
use kbf_types::LeaseId;
use prost_types::Timestamp;
use tokio::process::Child;
use tokio::sync::oneshot;

use crate::cgroup::{LeaseCgroup, SwapKill, memory_max, presses_into_swap};
use crate::image::{
    ImageCheckError, ImageRef, ManifestKind, PROPERTY, manifest_file, manifest_kind,
};
use crate::keeper::Keeper;
use crate::outputs::{OutputLimits, collect_log, collect_timed};
use crate::pending::{millis, phases_line};
use crate::podman::{
    CONTAINER_OWNER, ContainerLimits, ContainerSpec, DAEMON_OWNER, Ended, LEASE_PREFIX, Podman,
};
use crate::program;
use crate::remove::remove_tree;
use crate::tree::{
    TreeError, check_relative, fetch_message, materialize, materialize_cached, output_paths,
    prepare_outputs, refuse_hidden_working_directory,
};

/// The driver name in the node report.
pub const DRIVER: &str = "container";
/// The lease kind this driver runs.
pub const KIND: &str = "action";

/// Where and how the driver runs containers.
#[derive(Clone, Debug)]
pub struct PodmanConfig {
    /// The `podman` program.
    pub podman: PathBuf,
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
    /// Whose containers these are: each is labelled `kbf.owner=<owner>`
    /// ([`crate::podman::OWNER_LABEL`]), and at start the runtime removes every
    /// container so labelled. The daemon passes its node id. Two runtimes that share a
    /// Podman store need two owners, or each start removes the other's containers.
    pub owner: String,
    /// The node's input cache, if it has one (`--cache-dir`). Without it, a lease's
    /// input files are its own copies, as before the cache.
    pub cache: Option<CacheUse>,
}

/// The node's input cache as the driver is given it: not opened yet.
/// [`PodmanRuntime::new`] opens it once Podman's storage passed the cache's checks, so
/// on storage they refuse, the cache directory is never touched.
#[derive(Clone)]
pub struct CacheUse {
    pub config: CacheConfig,
    /// The `kbf-daemon` binary, run under `podman unshare` in its `__chown-dirs` mode
    /// ([`crate::chown_dirs`]) to give the input root's directories, not its files, to
    /// the container's root and back.
    pub helper: PathBuf,
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
}

impl PodmanConfig {
    /// A configuration for `owner`'s containers with `podman` from `PATH`, cgroup v2 at
    /// `/sys/fs/cgroup`, a one hour default timeout, a five second kill grace,
    /// the default [`OutputLimits`], [`kbf_daemon::UPLOAD_DEADLINE`],
    /// [`ContainerLimits::DEFAULT`] and
    /// [`SwapKill::DEFAULT`].
    #[must_use]
    pub fn new(scratch: PathBuf, cgroup_parent: String, owner: String) -> Self {
        Self {
            podman: PathBuf::from("podman"),
            scratch,
            cgroup_root: PathBuf::from("/sys/fs/cgroup"),
            cgroup_parent,
            default_timeout: Duration::from_secs(3600),
            kill_grace: Duration::from_secs(5),
            outputs: OutputLimits::DEFAULT,
            upload_deadline: kbf_daemon::UPLOAD_DEADLINE,
            limits: ContainerLimits::DEFAULT,
            swap_kill: SwapKill::DEFAULT,
            owner,
            cache: None,
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
        if self.owner.is_empty() {
            return Err(ConfigError::Owner);
        }
        let limits = self.limits;
        for (name, value) in [
            ("pids", limits.pids),
            ("shm", limits.shm_mib),
            ("nofile", limits.nofile),
            ("nproc", limits.nproc),
        ] {
            // Podman reads a pids limit of 0 as "unlimited", and a 0 for the others
            // would leave nothing to run with.
            if value == 0 {
                return Err(ConfigError::Limit(name));
            }
        }
        Ok(())
    }
}

/// Why a [`PodmanConfig`] cannot be used.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Podman's `--volume` syntax separates fields with `:` and options with `,`.
    #[error("scratch directory {0:?} must be absolute and contain no ':' or ','")]
    Scratch(PathBuf),
    #[error("cgroup parent {0:?} must start with '/'")]
    CgroupParent(String),
    #[error("the container owner must not be empty")]
    Owner,
    /// One of the [`ContainerLimits`], by name, is 0.
    #[error("the container {0} limit must be at least 1")]
    Limit(&'static str),
}

/// Why a [`PodmanRuntime`] does not start.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// What a previous daemon left could not be listed or removed. Starting anyway
    /// could run a lease's work beside its leftover container.
    #[error("removing what a previous daemon left: {0}")]
    Sweep(String),
}

/// How long the clean waits for a `podman start` it has sent `SIGKILL` to exit, and how
/// often it looks. A killed process exits once it leaves the system call it is in.
const REAP_LIMIT: Duration = Duration::from_secs(5);
const REAP_PAUSE: Duration = Duration::from_millis(2);

/// A killer waiting for a lease's work to stop.
type Stop = oneshot::Sender<()>;

/// A prepared lease: what `podman create` needs and what collecting needs.
struct Prepared {
    spec: ContainerSpec,
    outputs: Vec<String>,
    timeout: Duration,
}

/// Runs `action` leases in rootless Podman.
#[derive(Debug)]
pub struct PodmanRuntime<C> {
    config: PodmanConfig,
    podman: Podman,
    cas: Arc<C>,
    stops: Mutex<BTreeMap<LeaseId, oneshot::Sender<Stop>>>,
    /// The image store's per-image directories, asked of Podman once.
    images: tokio::sync::OnceCell<PathBuf>,
    /// The input cache leases link from, if the node has one and it could be used.
    cache: Option<Cached>,
    /// What the last lease's inputs cost, with the cache (a test's counter; each
    /// lease's are logged).
    last_inputs: Mutex<Option<InputStats>>,
}

impl<C: Cas> PodmanRuntime<C> {
    /// A runtime that reads and writes blobs through `cas`. Before it returns, so
    /// before the daemon says `Hello`, it removes what a previous daemon of the same
    /// owner left: every container labelled as the owner's (killing what still runs in
    /// it), and every lease scratch directory, each with its lease cgroup. Blocking.
    ///
    /// # Errors
    /// The configuration is unusable, or a leftover could not be listed or removed.
    /// With a cache, it then asks Podman for its graph driver, and opens the cache
    /// only on storage the cache's overlay checks passed on (the kernel's `overlay`,
    /// without a `mount_program` such as fuse-overlayfs). On other storage, or when the
    /// cache cannot be opened, it warns and runs uncached.
    pub fn new(config: PodmanConfig, cas: Arc<C>) -> Result<Self, StartError> {
        config.check()?;
        let mut runtime = Self {
            podman: Podman::new(config.podman.clone()),
            config,
            cas,
            stops: Mutex::new(BTreeMap::new()),
            images: tokio::sync::OnceCell::new(),
            cache: None,
            last_inputs: Mutex::new(None),
        };
        sweep(&runtime.podman, &runtime.config).map_err(StartError::Sweep)?;
        if let Some(given) = runtime.config.cache.clone() {
            runtime.cache = open_cache(&runtime.podman, given);
        }
        Ok(runtime)
    }

    /// Whether leases link their inputs from the cache.
    #[must_use]
    pub fn cached(&self) -> bool {
        self.cache.is_some()
    }

    /// What the last lease's inputs cost through the cache, if a lease used it.
    #[must_use]
    pub fn last_inputs(&self) -> Option<InputStats> {
        *self
            .last_inputs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn stops(&self) -> MutexGuard<'_, BTreeMap<LeaseId, oneshot::Sender<Stop>>> {
        // Each update is one map operation, so the map is consistent after a panic.
        self.stops.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Step 1: everything up to `podman create`, through the lease's `cas`.
    async fn prepare(
        &self,
        cas: &C::Lease,
        work: &Work,
        lease: &mut Lease,
    ) -> Result<Prepared, RuntimeError> {
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

        self.check_image(&image).await?;

        let root = lease.dir.join("root");
        let upper = lease.dir.join("upper");
        let overlay_work = lease.dir.join("work");
        for dir in [&lease.dir, &root, &upper, &overlay_work] {
            tokio::fs::create_dir(dir)
                .await
                .map_err(|e| failed(dir, &e))?;
        }
        match &self.cache {
            Some(cache) => {
                let private = private_paths(&command.working_directory, &outputs);
                let stats = materialize_cached(cas, &cache.cache, input_root, &root, &private)
                    .await
                    .map_err(tree_error)?;
                tracing::info!(lease = %lease.name, "inputs: {stats}");
                *self
                    .last_inputs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(stats);
            }
            None => materialize(cas, input_root, &root)
                .await
                .map_err(tree_error)?,
        }
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
        // The container's root is a subordinate id (`--userns=nomap`), which may write
        // only under directories it owns.
        self.give_overlay(CONTAINER_OWNER, &root, &upper, &overlay_work)
            .await?;
        // `actions/`'s own OOM count before the lease, to say afterwards whether its
        // limit killed the lease.
        lease.backstop_ooms = lease.cgroup.parent_ooms().ok();
        // The keeper, before anything runs in the cgroup: it kills the cgroup if the
        // daemon dies.
        let kill = lease.cgroup.dir().join("cgroup.kill");
        let keeper = (lease.cgroup.create(work.resources))
            .and_then(|()| Keeper::watch(&kill))
            .map_err(|e| failed(lease.cgroup.dir(), &e))?;
        lease.keeper = Some(keeper);

        let spec = ContainerSpec {
            name: lease.name.clone(),
            owner: self.config.owner.clone(),
            image: image.to_string(),
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
        // future drops any Podman query with it (`kill_on_drop`), and the clean step
        // removes whatever was written.
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
        // Execution counts from `podman create`: making the container is part of
        // running the command in one.
        clock.execution_start_timestamp = now();
        lease.created = true;
        self.podman
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
                .podman
                .start(&lease.name, stdout, stderr)
                .map_err(RuntimeError::Failed)?;
            // The lease owns `podman start` from here, so a dropped run's clean reaps it.
            let child = lease.start.insert(start);

            tokio::select! {
                // `podman start`'s own status is not the action's: Podman's record, read
                // below, is. An error waiting for it is not trusted either way, since
                // `ended` fails the lease unless that record says the container exited, or crun
                // reports it could not start the program (`program::not_run`).
                _ = child.wait() => {}
                () = tokio::time::sleep(timeout) => {
                    self.stop_container(&lease.name, &lease.cgroup, child).await;
                    return Err(self.timed_out(cas, &lease.name, &stdout_path, &stderr_path).await);
                }
                Ok(by) = &mut *stop => {
                    *killer = Some(by);
                    self.stop_container(&lease.name, &lease.cgroup, child).await;
                    return Err(RuntimeError::Killed);
                }
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

            let ended = self.podman.ended(&lease.name).await;
            // An exec that failed with ETXTBSY (`program::text_file_busy`) is started once
            // more: the write descriptor that caused it is gone by now.
            if started == 1
                && matches!(ended, Ok(Ended::NotRun(_)))
                && program::text_file_busy(&stderr_path).await
            {
                tracing::warn!(lease = %lease.name, "exec found the program busy (ETXTBSY); again");
                continue;
            }
            break ended;
        };
        // SIGKILL (137): whether the kernel's OOM killer did it, and for whose limit,
        // is read from the lease cgroup, not from Podman. The same is read when Podman
        // recorded no exit or could not be asked: the lease cgroup's counters do not
        // depend on Podman's record, so a kill they show is reported as one.
        if matches!(ended, Ok(Ended::Exited(137) | Ended::NotRun(_)) | Err(_)) {
            match lease.cgroup.oom_outcome(cap, lease.backstop_ooms) {
                Ok(Some(kill)) => return Err(kill),
                Ok(None) => {}
                Err(e) if ended == Ok(Ended::Exited(137)) => {
                    return Err(failed(lease.cgroup.dir(), &e));
                }
                // No exit code either: what Podman said is the one to report.
                Err(_) => {}
            }
        }
        let exit_code = match ended.map_err(RuntimeError::Failed)? {
            Ended::Exited(code) => code,
            Ended::NotRun(state) => program::not_run(&spec, &stderr_path, &state).await?,
        };
        clock.execution_completed_timestamp = now();
        // The upload counts from the chown, which makes the outputs readable to it.
        clock.output_upload_start_timestamp = now();
        // The container has exited: hand its files back to the daemon's user, so an
        // output the action left unreadable to others is still read, as its owner.
        self.give_overlay(
            DAEMON_OWNER,
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
            // The outputs the server lacks go through a spool in the lease's scratch
            // directory (the daemon's, outside the container) as whole segments.
            let timings = collect_timed(
                cas,
                &spec.upper,
                &lease.dir,
                &spec.working_directory,
                &outputs,
                self.config.outputs,
                &mut result,
            )
            .await
            .map_err(outputs_error)?;
            let max = self.config.outputs.max_stdio_bytes;
            let logs = Instant::now();
            for (path, slot) in [
                (&stdout_path, &mut result.stdout_digest),
                (&stderr_path, &mut result.stderr_digest),
            ] {
                *slot = Some(collect_log(cas, path, max).await.map_err(outputs_error)?);
            }
            tracing::info!(
                lease = %lease.name,
                "outputs phases: {}",
                phases_line(&timings, &cas.upload_stats(), millis(logs))
            );
            Ok(())
        };
        upload_within(self.config.upload_deadline, upload).await?;
        clock.output_upload_completed_timestamp = now();
        clock.worker_completed_timestamp = now();
        result.execution_metadata = Some(clock);
        Ok(result)
    }

    /// Gives the overlay's directories to `owner`. Without the cache, everything under
    /// them. With it, only the input root's directories: its files are links to the
    /// cache's entries, which must stay the daemon's (unmapped in the container, so
    /// the action cannot write them); the upper and work directories hold only the
    /// lease's own files.
    async fn give_overlay(
        &self,
        owner: &str,
        root: &Path,
        upper: &Path,
        work: &Path,
    ) -> Result<(), RuntimeError> {
        let given = match &self.cache {
            Some(cache) => {
                let dirs = self.podman.chown_dirs(&cache.helper, owner, root).await;
                match dirs {
                    Ok(()) => self.podman.chown(owner, &[upper, work]).await,
                    Err(e) => Err(e),
                }
            }
            None => self.podman.chown(owner, &[root, upper, work]).await,
        };
        given.map_err(RuntimeError::Failed)
    }

    /// Checks that this node's image store holds `image` and that its digest names one
    /// image, not an index: by reading the manifest the store keeps under that digest.
    async fn check_image(&self, image: &ImageRef) -> Result<(), RuntimeError> {
        self.check_pinned(image).await.map_err(|e| match e {
            ImageCheckError::Index(_) => RuntimeError::Invalid(e.to_string()),
            ImageCheckError::Missing(_) | ImageCheckError::Failed(_) => {
                RuntimeError::Failed(e.to_string())
            }
        })
    }

    /// Checks, as each lease's prepare step does, that this node's image store holds
    /// `image` and that its digest names one architecture's manifest, not an index. The
    /// daemon checks each image it is pinned to (`--image`) once at start with it.
    pub async fn check_pinned(&self, image: &ImageRef) -> Result<(), ImageCheckError> {
        let Some(id) = self
            .podman
            .image_id(&image.to_string())
            .await
            .map_err(ImageCheckError::Failed)?
        else {
            return Err(ImageCheckError::Missing(image.clone()));
        };
        let images = self
            .images
            .get_or_try_init(|| self.podman.images_dir())
            .await
            .map_err(ImageCheckError::Failed)?;
        let path = images.join(&id).join(manifest_file(image.digest()));
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            ImageCheckError::Failed(format!(
                "the image store holds no manifest {} for image {id}: {e}",
                image.digest()
            ))
        })?;
        match manifest_kind(&bytes, image.digest()).map_err(ImageCheckError::Failed)? {
            ManifestKind::Image => Ok(()),
            ManifestKind::Index => Err(ImageCheckError::Index(image.clone())),
        }
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
    /// grace), then the grace period for Podman to notice. Returns once `podman start`
    /// has ended.
    async fn kill_container(&self, name: &str, cgroup: &LeaseCgroup, child: &mut Child) {
        if let Err(e) = cgroup.kill() {
            tracing::warn!(lease = %name, "cgroup.kill: {e}");
        }
        if tokio::time::timeout(self.config.kill_grace, child.wait())
            .await
            .is_err()
        {
            // The container is removed by force in the clean step either way.
            let _ = child.kill().await;
        }
    }

    /// The kill path: SIGTERM, the grace period, `cgroup.kill`, then the
    /// grace period again for Podman to notice. Returns once `podman start` has ended.
    async fn stop_container(&self, name: &str, cgroup: &LeaseCgroup, child: &mut Child) {
        let grace = self.config.kill_grace;
        if let Err(e) = self.podman.signal(name, "TERM").await {
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
            let _ = child.kill().await;
        }
    }
}

impl<C: Cas> Runtime for PodmanRuntime<C> {
    fn driver(&self) -> &'static str {
        DRIVER
    }

    fn serves(&self, kind: &str) -> bool {
        kind == KIND
    }

    async fn run(&self, work: Work) -> Result<ActionResult, RuntimeError> {
        let (stop_tx, mut stop) = oneshot::channel();
        let _registered = Registered::new(self, work.lease_id, stop_tx);
        let mut lease = Lease::new(&self.podman, &self.config, work.lease_id);
        let mut killer = None;
        let outcome = self
            .attempt(&work, &mut lease, &mut stop, &mut killer)
            .await;
        let cleaned = lease.clean().await;
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
        // (and cleaned) without seeing the kill, `tx` is dropped with the failed send
        // or with the run's receiver, and `rx` returns at once.
        let _ = stop.send(tx);
        let _ = rx.await;
    }
}

/// A lease's entry in the kill table, removed when its run ends or is dropped.
struct Registered<'a, C> {
    runtime: &'a PodmanRuntime<C>,
    id: LeaseId,
}

impl<'a, C: Cas> Registered<'a, C> {
    fn new(runtime: &'a PodmanRuntime<C>, id: LeaseId, stop: oneshot::Sender<Stop>) -> Self {
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

/// Removes every lease a previous daemon of `config`'s owner left: the union of the
/// owner's containers and the lease scratch directories, each removed as a lease's
/// clean removes it (`cgroup.kill`, `podman rm --force`, the lease cgroup, the scratch
/// directory). Blocking.
fn sweep(podman: &Podman, config: &PodmanConfig) -> Result<(), String> {
    let mut names: BTreeSet<String> = podman.owned(&config.owner)?.into_iter().collect();
    match std::fs::read_dir(&config.scratch) {
        Ok(entries) => names.extend(
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with(LEASE_PREFIX)),
        ),
        // Absent until the first lease.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", config.scratch.display())),
    }
    let mut errors = Vec::new();
    for name in names {
        tracing::warn!(lease = %name, "removing a lease a previous daemon left");
        let mut lease = Lease::named(podman, config, name);
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
    podman: Podman,
    /// `kbf-lease-<term>-<seq>`: the container's name and the lease cgroup's.
    name: String,
    /// The scratch directory.
    dir: PathBuf,
    cgroup: LeaseCgroup,
    /// `actions/`'s own OOM count when the lease cgroup was made, if it could be read.
    backstop_ooms: Option<u64>,
    /// Whether a container may exist.
    created: bool,
    /// `podman start --attach`, once started. Reaped before anything is removed.
    start: Option<Child>,
    /// Kills the lease cgroup if the daemon dies, once the cgroup is made. Removed
    /// last, once the lease is cleaned.
    keeper: Option<Keeper>,
    /// Whether cleaning is still owed.
    armed: bool,
}

impl Lease {
    fn new(podman: &Podman, config: &PodmanConfig, id: LeaseId) -> Self {
        Self::named(
            podman,
            config,
            format!("{LEASE_PREFIX}{}-{}", id.term, id.seq),
        )
    }

    /// The lease named `name` (`kbf-lease-<term>-<seq>`), nothing of it created yet.
    fn named(podman: &Podman, config: &PodmanConfig, name: String) -> Self {
        Self {
            podman: podman.clone(),
            dir: config.scratch.join(&name),
            cgroup: LeaseCgroup::at(&config.cgroup_root, &config.cgroup_parent, &name),
            backstop_ooms: None,
            name,
            created: false,
            start: None,
            keeper: None,
            armed: true,
        }
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
        // First, `podman start`: a dropped run has not waited for it, and until it has
        // exited it may still start the container's processes or write into the lease.
        if let Some(mut start) = self.start.take() {
            errors.extend(reap(&mut start, REAP_LIMIT, REAP_PAUSE).err());
        }
        if self.created {
            // Kill whatever runs in the lease first: a run dropped mid-start leaves
            // `crun create` in the container's cgroup, and `podman rm --force` returns
            // without waiting for it, so the cgroup stays busy.
            let _ = self.cgroup.kill();
            if let Err(e) = self.podman.remove_blocking(&self.name) {
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
            // permissions left, can only be removed inside Podman's user namespace.
            Err(first) => {
                if let Err(e) = self.podman.unshare_remove_blocking(&self.dir) {
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

/// Kills `child` unless it has exited, and waits at most `limit` for it to exit and be
/// reaped. Blocking, without the async runtime: a dropped run's clean runs inside `Drop`.
fn reap(child: &mut Child, limit: Duration, pause: Duration) -> Result<(), String> {
    // Killed only while `try_wait` says it is unreaped: until then its pid can name no
    // other process. `try_wait`, not the kill's result, then says when it is gone.
    // PID reuse: once `try_wait` has reaped the child, tokio's `Child` is done and
    // `start_kill` sends nothing, so no signal reaches a process that took the pid
    // after. Keep this on `Child`; a raw pid (`kill(2)`, `waitpid`) loses that.
    if child.try_wait().map_err(wait_failed)?.is_none() {
        let _ = child.start_kill();
    }
    wait_exited(&mut || child.try_wait(), limit, pause)
}

fn wait_failed(error: std::io::Error) -> String {
    format!("wait for podman start: {error}")
}

/// Calls `try_wait` every `pause` until it reports an exit, fails, or `limit` has passed.
fn wait_exited(
    try_wait: &mut dyn FnMut() -> std::io::Result<Option<std::process::ExitStatus>>,
    limit: Duration,
    pause: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + limit;
    loop {
        match try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(pause),
            Ok(None) => return Err(format!("podman start still runs {limit:?} after SIGKILL")),
            Err(e) => return Err(wait_failed(e)),
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

/// The cache `given` opened, once Podman's storage passed the cache's checks; `None`,
/// after a warning, when it did not or the cache cannot be opened. Blocking.
fn open_cache(podman: &Podman, given: CacheUse) -> Option<Cached> {
    let shown = given.config.dir.display().to_string();
    let opened = cache_usable(podman).and_then(|()| Cache::open(given.config));
    match opened {
        Ok(cache) => {
            let stats = cache.stats();
            let (bytes, entries, mode) = (stats.bytes, stats.entries, cache.link_mode());
            tracing::info!("cache: {shown}: {bytes} bytes in {entries} entries, link mode {mode}");
            let helper = given.helper;
            Some(Cached { cache, helper })
        }
        Err(why) => {
            let reason = why.reason();
            tracing::warn!("cache: off:{reason} ({shown}: {why}); leases run uncached");
            None
        }
    }
}

/// Whether Podman's storage is one the cache's overlay checks passed on: the kernel's
/// `overlay` graph driver, with no `mount_program` (fuse-overlayfs) in its options.
/// Blocking.
fn cache_usable(podman: &Podman) -> Result<(), Disabled> {
    let (driver, options) = podman.graph_driver().map_err(Disabled::GraphDriver)?;
    if driver != "overlay" || options.get("overlay.mount_program").is_some() {
        return Err(Disabled::GraphDriver(format!("{driver} {options}")));
    }
    Ok(())
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
