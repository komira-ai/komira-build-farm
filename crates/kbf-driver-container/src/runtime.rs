//! [`PodmanRuntime`]: one action, one fresh rootless Podman container, never reused.
//!
//! Each lease goes through the six driver steps (RFC 10.1):
//! 1. **prepare:** fetch the Action and Command, check the image and paths, write the
//!    input root into the lease's scratch directory, refuse outputs that are inputs,
//!    give the overlay's directories to the container's root (a subordinate id, see
//!    `podman::create_args`), make the lease cgroup;
//! 2. **start:** `podman create`, then `podman start --attach`;
//! 3. **watch:** wait for the exit, the timeout, or [`Runtime::kill`];
//! 4. **collect:** the exit code from Podman's record, OOM from the lease cgroup's
//!    `memory.events`; the overlay's directories back to the daemon's user; outputs,
//!    stdout and stderr into the CAS;
//! 5. **clean:** remove the container, the lease cgroup and the scratch directory;
//! 6. **verify-clean:** neither directory may remain.
//!
//! Cleaning runs on every path out of `run`: success, failure, timeout and kill. If the
//! daemon drops the `run` future instead (its task is cancelled), the lease's `Drop`
//! cleans up, blocking, before the future is gone. Either way the clean first makes
//! sure `podman start` has exited and been reaped, so nothing the driver started is
//! left running while it removes the lease. A clean that fails turns the lease into a
//! failure: a dirty node must be loud.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use kbf_daemon::cas::Cas;
use kbf_daemon::{Runtime, RuntimeError, Work};
use kbf_proto::reapi::{Action, ActionResult, Command, Platform};
use kbf_types::LeaseId;
use tokio::process::Child;
use tokio::sync::oneshot;

use crate::cgroup::LeaseCgroup;
use crate::image::{ImageRef, ManifestKind, PROPERTY, manifest_file, manifest_kind};
use crate::outputs::{OutputLimits, collect_log};
use crate::podman::{CONTAINER_OWNER, ContainerSpec, DAEMON_OWNER, LEASE_PREFIX, Podman};
use crate::remove::remove_tree;
use crate::tree::{
    TreeError, check_relative, collect, fetch_message, materialize, output_paths,
    refuse_hidden_working_directory, refuse_outputs_in_inputs,
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
    /// How long a container gets after SIGTERM before `cgroup.kill` (RFC 10.10).
    pub kill_grace: Duration,
    /// How much output one action may leave; past it, the action fails.
    pub outputs: OutputLimits,
    /// Whose containers these are: each is labelled `kbf.owner=<owner>`
    /// ([`crate::podman::OWNER_LABEL`]), and at start the runtime removes every
    /// container so labelled. The daemon passes its node id. Two runtimes that share a
    /// Podman store need two owners, or each start removes the other's containers.
    pub owner: String,
}

impl PodmanConfig {
    /// A configuration for `owner`'s containers with `podman` from `PATH`, cgroup v2 at
    /// `/sys/fs/cgroup`, a one hour default timeout, the RFC's five second kill grace and
    /// the default [`OutputLimits`].
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
            owner,
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
}

impl<C: Cas> PodmanRuntime<C> {
    /// A runtime that reads and writes blobs through `cas`. Before it returns, so
    /// before the daemon says `Hello`, it removes what a previous daemon of the same
    /// owner left: every container labelled as the owner's (killing what still runs in
    /// it), and every lease scratch directory, each with its lease cgroup. Blocking.
    ///
    /// # Errors
    /// The configuration is unusable, or a leftover could not be listed or removed.
    pub fn new(config: PodmanConfig, cas: Arc<C>) -> Result<Self, StartError> {
        config.check()?;
        let runtime = Self {
            podman: Podman::new(config.podman.clone()),
            config,
            cas,
            stops: Mutex::new(BTreeMap::new()),
            images: tokio::sync::OnceCell::new(),
        };
        sweep(&runtime.podman, &runtime.config).map_err(StartError::Sweep)?;
        Ok(runtime)
    }

    fn stops(&self) -> MutexGuard<'_, BTreeMap<LeaseId, oneshot::Sender<Stop>>> {
        // Each update is one map operation, so the map is consistent after a panic.
        self.stops.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Step 1: everything up to `podman create`.
    async fn prepare(&self, work: &Work, lease: &mut Lease) -> Result<Prepared, RuntimeError> {
        let cas = &*self.cas;
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
        materialize(cas, input_root, &root)
            .await
            .map_err(tree_error)?;
        // The driver makes the working directory in the upper layer, which would hide
        // an input symlink or file of that name: refused rather than run elsewhere.
        refuse_hidden_working_directory(&root, &command.working_directory)
            .await
            .map_err(tree_error)?;
        // Outputs are read from the upper layer, so none may already be an input.
        refuse_outputs_in_inputs(&root, &command.working_directory, &outputs)
            .await
            .map_err(tree_error)?;
        // REAPI: the worker makes the working directory and each output's parent.
        let out_dir = upper.join(&command.working_directory);
        let parents = outputs
            .iter()
            .filter_map(|output| Path::new(output).parent().map(|p| out_dir.join(p)));
        for dir in std::iter::once(out_dir.clone()).chain(parents) {
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| failed(&dir, &e))?;
        }
        // The container's root is a subordinate id (`--userns=nomap`), which may write
        // only under directories it owns.
        self.podman
            .chown(CONTAINER_OWNER, &[&root, &upper, &overlay_work])
            .await
            .map_err(RuntimeError::Failed)?;
        lease
            .cgroup
            .create(work.resources)
            .map_err(|e| failed(lease.cgroup.dir(), &e))?;

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
        // A kill during prepare (a long input fetch) stops it at once: dropping the
        // future drops any Podman query with it (`kill_on_drop`), and the clean step
        // removes whatever was written.
        let Prepared {
            spec,
            outputs,
            timeout,
        } = tokio::select! {
            prepared = self.prepare(work, lease) => prepared?,
            Ok(by) = &mut *stop => {
                *killer = Some(by);
                return Err(RuntimeError::Killed);
            }
        };
        let cas = &*self.cas;
        lease.created = true;
        self.podman
            .create(&spec)
            .await
            .map_err(RuntimeError::Failed)?;
        let stdout_path = lease.dir.join("stdout");
        let stderr_path = lease.dir.join("stderr");
        let stdout = std::fs::File::create(&stdout_path).map_err(|e| failed(&stdout_path, &e))?;
        let stderr = std::fs::File::create(&stderr_path).map_err(|e| failed(&stderr_path, &e))?;
        let start = self
            .podman
            .start(&lease.name, stdout, stderr)
            .map_err(RuntimeError::Failed)?;
        // The lease owns `podman start` from here, so a dropped run's clean reaps it.
        let child = lease.start.insert(start);

        tokio::select! {
            // `podman start`'s own status is not the action's: Podman's record, read
            // below, is. An error waiting for it is not trusted either way, since
            // `exit_code` fails the lease unless that record says the container exited.
            _ = child.wait() => {}
            () = tokio::time::sleep(timeout) => {
                self.stop_container(&lease.name, &lease.cgroup, child).await;
                return Err(RuntimeError::TimedOut);
            }
            Ok(by) = &mut *stop => {
                *killer = Some(by);
                self.stop_container(&lease.name, &lease.cgroup, child).await;
                return Err(RuntimeError::Killed);
            }
        }

        let exit_code = self
            .podman
            .exit_code(&lease.name)
            .await
            .map_err(RuntimeError::Failed)?;
        // 137 is SIGKILL. Whether the kernel's OOM killer sent it is read from the
        // lease cgroup, not from Podman.
        if exit_code == 137 {
            let kills = lease
                .cgroup
                .oom_kills()
                .map_err(|e| failed(lease.cgroup.dir(), &e))?;
            if kills > 0 {
                return Err(RuntimeError::Failed(format!(
                    "the kernel OOM killer ended the action (oom_kill {kills} in the lease cgroup)"
                )));
            }
        }
        // The container has exited: hand its files back to the daemon's user, so an
        // output the action left unreadable to others is still read, as its owner.
        self.podman
            .chown(
                DAEMON_OWNER,
                &[&spec.input_root, &spec.upper, &spec.overlay_work],
            )
            .await
            .map_err(RuntimeError::Failed)?;
        let mut result = ActionResult {
            exit_code,
            ..ActionResult::default()
        };
        collect(
            cas,
            &spec.upper,
            &spec.working_directory,
            &outputs,
            self.config.outputs,
            &mut result,
        )
        .await
        .map_err(tree_error)?;
        let max = self.config.outputs.max_stdio_bytes;
        for (path, slot) in [
            (&stdout_path, &mut result.stdout_digest),
            (&stderr_path, &mut result.stderr_digest),
        ] {
            *slot = Some(collect_log(cas, path, max).await.map_err(tree_error)?);
        }
        Ok(result)
    }

    /// Checks that this node's image store holds `image` and that its digest names one
    /// image, not an index: by reading the manifest the store keeps under that digest.
    async fn check_image(&self, image: &ImageRef) -> Result<(), RuntimeError> {
        let Some(id) = self
            .podman
            .image_id(&image.to_string())
            .await
            .map_err(RuntimeError::Failed)?
        else {
            return Err(RuntimeError::Failed(format!(
                "image {image} is not in this node's image store"
            )));
        };
        let images = self
            .images
            .get_or_try_init(|| self.podman.images_dir())
            .await
            .map_err(RuntimeError::Failed)?;
        let path = images.join(&id).join(manifest_file(image.digest()));
        let bytes = tokio::fs::read(&path).await.map_err(|e| {
            RuntimeError::Failed(format!(
                "the image store holds no manifest {} for image {id}: {e}",
                image.digest()
            ))
        })?;
        match manifest_kind(&bytes, image.digest()).map_err(RuntimeError::Failed)? {
            ManifestKind::Image => Ok(()),
            ManifestKind::Index => Err(RuntimeError::Invalid(format!(
                "container-image {image} names an image index; name the per-architecture \
                 manifest digest"
            ))),
        }
    }

    /// The kill path (RFC 10.10): SIGTERM, the grace period, `cgroup.kill`, then the
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
    /// Whether a container may exist.
    created: bool,
    /// `podman start --attach`, once started. Reaped before anything is removed.
    start: Option<Child>,
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
            name,
            created: false,
            start: None,
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

fn tree_error(error: TreeError) -> RuntimeError {
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

fn failed(path: &Path, error: &std::io::Error) -> RuntimeError {
    RuntimeError::Failed(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::platform::Property;

    use super::*;

    fn platform(value: &str) -> Option<Platform> {
        Some(Platform {
            properties: vec![Property {
                name: PROPERTY.to_owned(),
                value: value.to_owned(),
            }],
        })
    }

    fn image(hex: char) -> String {
        format!("docker://busybox@sha256:{}", hex.to_string().repeat(64))
    }

    /// Catches the deprecated Command platform overriding the Action's, and an action
    /// with no image running in whatever image a node picks.
    #[test]
    #[allow(deprecated)]
    fn the_image_comes_from_the_action_then_the_command() {
        let command = Command {
            platform: platform(&image('c')),
            ..Command::default()
        };
        let action = Action {
            platform: platform(&image('a')),
            ..Action::default()
        };
        let chosen = image_of(&action, &command).expect("image");
        assert_eq!(chosen.digest(), format!("sha256:{}", "a".repeat(64)));
        let chosen = image_of(&Action::default(), &command).expect("image");
        assert_eq!(chosen.digest(), format!("sha256:{}", "c".repeat(64)));
        let none = image_of(&Action::default(), &Command::default());
        assert!(matches!(none, Err(RuntimeError::Invalid(_))));
        let tagged = Action {
            platform: platform("docker://busybox:latest"),
            ..Action::default()
        };
        assert!(matches!(
            image_of(&tagged, &command),
            Err(RuntimeError::Invalid(_))
        ));
    }

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
        assert!(matches!(
            timeout_of(&with(1, -1), default),
            Err(RuntimeError::Invalid(_))
        ));
    }

    /// Catches a scratch path Podman's `--volume` syntax would split, a cgroup parent
    /// Podman would read relative to its own default, and an empty owner (whose sweep
    /// would look for containers labelled `kbf.owner=`). A usable one starts, its sweep
    /// asking a `podman` that knows no container (`true`).
    #[test]
    fn unusable_configurations_are_refused() {
        let cas = Arc::new(crate::cas::MemoryCas::new());
        let mut ok = PodmanConfig::new(
            PathBuf::from("/nonexistent/scratch"),
            "/actions".to_owned(),
            "node-1".to_owned(),
        );
        ok.podman = PathBuf::from("/bin/true");
        assert!(PodmanRuntime::new(ok.clone(), Arc::clone(&cas)).is_ok());
        let refused = |config: PodmanConfig| PodmanRuntime::new(config, Arc::clone(&cas)).err();
        for scratch in ["relative", "/a:b", "/a,b"] {
            let mut config = ok.clone();
            config.scratch = PathBuf::from(scratch);
            assert_eq!(
                refused(config),
                Some(ConfigError::Scratch(PathBuf::from(scratch)).into())
            );
        }
        let mut config = ok.clone();
        config.cgroup_parent = "actions".to_owned();
        assert_eq!(
            refused(config),
            Some(ConfigError::CgroupParent("actions".to_owned()).into())
        );
        let mut config = ok;
        config.owner.clear();
        assert_eq!(refused(config), Some(ConfigError::Owner.into()));
    }

    /// Catches a clean that removes the lease while `podman start` still runs (kbf
    /// #157): the process is killed and reaped before `reap` returns, and one already
    /// waited for is not an error.
    #[tokio::test]
    async fn reap_kills_and_reaps_a_running_child() {
        let mut child = tokio::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("pid");
        reap(&mut child, Duration::from_secs(5), Duration::from_millis(1)).expect("reaped");
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "sleep {pid} still there after reap"
        );
        let mut done = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        done.wait().await.expect("wait");
        reap(&mut done, Duration::ZERO, Duration::ZERO).expect("an exited child");
    }

    /// Catches an unbounded wait for a process that does not exit (one stuck in the
    /// kernel exits only once it leaves the call), and a failed wait taken as an exit.
    #[test]
    fn the_wait_for_podman_start_is_bounded() {
        let mut looks = 0;
        let mut never = || {
            looks += 1;
            Ok(None)
        };
        let limit = Duration::from_millis(20);
        let why = wait_exited(&mut never, limit, Duration::from_millis(1)).expect_err("never");
        assert!(why.contains("still runs"), "{why}");
        assert!(looks > 1, "gave up after {looks} looks");
        let mut failing = || Err(std::io::Error::other("no child"));
        let why = wait_exited(&mut failing, Duration::from_secs(5), Duration::ZERO)
            .expect_err("the wait failed");
        assert!(why.starts_with("wait for podman start: "), "{why}");
    }

    /// Catches a clean that reports success when it could not wait for `podman start`
    /// (here another reaper took its exit).
    #[tokio::test]
    async fn a_clean_that_cannot_wait_for_podman_start_fails() {
        let child = tokio::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let raw = i32::try_from(child.id().expect("pid")).expect("pid fits");
        let pid = rustix::process::Pid::from_raw(raw);
        rustix::process::waitpid(pid, rustix::process::WaitOptions::empty())
            .expect("reaped elsewhere");
        let mut config = PodmanConfig::new(
            PathBuf::from("/nonexistent-kbf-157/scratch"),
            "/actions".to_owned(),
            "node-1".to_owned(),
        );
        config.cgroup_root = PathBuf::from("/nonexistent-kbf-157/cgroup");
        let id = LeaseId { term: 1, seq: 1 };
        let mut lease = Lease::new(&Podman::new(config.podman.clone()), &config, id);
        lease.start = Some(child);
        let why = lease.clean_blocking().expect_err("the wait failed");
        assert!(why.starts_with("wait for podman start: "), "{why}");
    }

    /// Catches a clean step whose blocking task panicked being reported without saying
    /// it was the clean that failed (the lease then fails INTERNAL with this text).
    #[tokio::test]
    async fn a_clean_task_that_did_not_finish_names_the_clean() {
        let panicked = tokio::task::spawn_blocking(|| panic!("clean panicked"))
            .await
            .expect_err("the task panicked");
        let why = clean_task_failed(panicked);
        assert!(why.starts_with("clean task: "), "{why}");
    }
}
