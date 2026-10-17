//! `kbf-node`: the worker daemon as a library. The `kbf-daemon` binary is [`dispatch_helpers`],
//! [`Cli`] parsed from the command line, and [`start`]; another binary that runs the
//! daemon beside the server parses [`Cli`] the same way (it is a `clap::Args` too) and calls
//! the same functions. See the `kbf-daemon` library for what the daemon does.
//!
//! Configuration is by flags only (`kbf-daemon --help`). `--driver` picks how leases
//! run: `fake` (nothing runs; for bring-up), `container` (rootless Podman; Linux) or
//! `native` (plain processes; for Macs), and for `whole_machine` leases on a Mac
//! `native-whole-machine` (each lease as its own user in its GUI session, through
//! `kbf-mac-session`; see `whole_machine`). Given more than once, the daemon has one
//! runtime per driver (the node's own, native or container, set up first) and runs
//! each Start on the one that serves its lease kind; it refuses to start when two
//! serve the same kind. The two real drivers read and write blobs over mutual TLS
//! through the server's worker listener named by `--cas` (normally the same address
//! as `--server`), with the daemon's own certificate, and make lease directories
//! under `<--data-dir>/leases`. A Mac in komira's pool runs, for example:
//!
//! ```text
//! kbf-daemon --driver native --server https://kbf-server:8981 --cas https://kbf-server:8981 \
//!   --ca-cert ca.pem --cert node.pem --key node.key --node-id mac-studio-1 \
//!   --data-dir /var/kbf --label pool=darwin-sized
//! ```
//!
//! Exits non-zero when the node cannot be detected or the configuration cannot be
//! loaded; otherwise runs until SIGTERM or SIGINT, then exits zero. Leases still
//! running then are abandoned (their processes killed, their directories removed);
//! the scheduler places them again.
//!
//! Everything the daemon keeps on disk is under one directory, `--data-dir` (default
//! `/var/lib/kbf`; `data_dir`): `leases/`, and under the container driver and the
//! native driver on macOS the input cache it links (container) or clones (native) each
//! lease's input files from, and the shared input trees. Their budgets
//! come from the filesystem, not from flags; `--data-max-bytes` only lowers them.
//!
//! The log goes to stderr, in color only when stderr is a terminal: under launchd or
//! systemd it is a file or a journal, where escape codes are noise (issue #170). The
//! daemon does not rotate it; the service manager that owns stderr does. systemd's
//! journal rotates by its own size limits; launchd's `StandardErrorPath` file is
//! emptied by `kbf-mac-provision` at each restart and upgrade (see
//! `docs/design/mac-node-provisioning.md`, section 5.7).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

pub use kbf_daemon::LocalMember;
use kbf_daemon::{
    Args, Capacity, DAEMON_VERSION, Daemon, DaemonConfig, DriverWatch, FakeRuntime, FarmCas,
    Liveness, MapHolder, NodeReport, Runtime, Runtimes,
};
use kbf_driver_native::slots::Pool;
use kbf_driver_native::{MemoryPolicy, NativeConfig, NativeRuntime, xcode, xcode_watch};
use kbf_outputs::OutputLimits;
use kbf_threads::disk::Statvfs;
use kbf_transfer::Cas;
use tokio::signal::unix::{Signal, SignalKind, signal};

pub use data_dir::DEFAULT_DATA_DIR;
pub use run::run;

/// The `kbf-daemon` command line.
#[derive(Clone, Debug, clap::Parser)]
#[command(name = "kbf-daemon", version = DAEMON_VERSION, about = "The kbf worker daemon.")]
pub struct Cli {
    #[command(flatten)]
    daemon: Args,
    /// How leases run; once per runtime. The daemon runs each lease on the driver that
    /// serves its kind and refuses to start when two serve the same kind. The node's own
    /// driver (native or container) is set up first, whatever the order given.
    #[arg(long = "driver", value_enum, required = true)]
    drivers: Vec<Driver>,
    /// The server's worker listener, an `https://` URL (normally `--server`'s), that
    /// the container and native drivers read and write blobs through, over mutual TLS
    /// with this daemon's `--ca-cert`, `--cert`, `--key` and `--tls-server-name`.
    /// Plain `http://` is refused: the worker listener admits each blob call by the
    /// daemon's certificate. Every driver's blobs go through one transfer engine, over
    /// one connection, within the `--transfer-*` limits.
    #[arg(long)]
    cas: Option<String>,
    /// The update directory: the daemon fetches the release the frontend offers into
    /// its `inbox/`, over the connection `--cas` names, and reads the root helper's
    /// `status.json` there. Left out, the node updates by hand: it says so and touches
    /// no directory. Not taken by the all-in-one binary yet.
    #[arg(long, value_name = "DIR")]
    update_dir: Option<PathBuf>,
    #[command(flatten)]
    data: data_dir::DataArgs,
    #[command(flatten)]
    outputs: OutputLimits,
    /// `--reserve-cpus`: the logical CPUs the farm's processes keep; the leases get the
    /// rest of the CPUs the daemon may use, and the daemon's async, blocking and CPU
    /// pools are sized from it (`--async-threads`, `--blocking-threads` and
    /// `--cpu-threads` override them). See `kbf_threads`.
    #[command(flatten)]
    threads: kbf_threads::ThreadArgs,
    /// How long one lease's whole output upload (its output files, stdout and stderr)
    /// may take, in seconds, before the lease fails with a reason naming the upload and
    /// the time it ran (container and native drivers). Without it, a store that stops
    /// answering holds the lease, and its slot, with no error.
    #[arg(
        long,
        default_value_t = kbf_daemon::UPLOAD_DEADLINE.as_secs(),
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    output_upload_deadline_secs: u64,
    #[command(flatten)]
    transfer: transfer::TransferArgs,
    /// The cgroup lease cgroups are made under, from the cgroup root (container). Left
    /// out, the daemon sets up its own cgroup, which its systemd unit must delegate
    /// (`Delegate=yes`): it moves itself into a `supervisor` leaf there and makes
    /// `actions` beside it, with cpu, memory and pids enabled. Given, it must already
    /// enable cpu, memory and pids for its children.
    #[arg(long)]
    cgroup_parent: Option<String>,
    /// Written to the actions cgroup's `memory.max` at start (container): the most
    /// memory all leases together may use, in GiB, so the rest stays for what else
    /// runs on the node. The node reports the lowest `memory.max` above its leases, or
    /// its memory when that is less, and the CPUs of their cpuset.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    actions_memory_max_gib: Option<u64>,
    /// Written to the daemon's `supervisor` leaf's `memory.min` at start, in MiB
    /// (container, without `--cgroup-parent`): memory the kernel does not reclaim from
    /// the daemon while it uses no more, so builds short of memory never take it. The
    /// kernel protects no more than the unit's and its slice's `MemoryMin=` allow; the
    /// daemon warns when one is lower. 0 protects nothing.
    #[arg(long, default_value_t = 256)]
    supervisor_memory_min_mib: u64,
    /// Where cgroup v2 is mounted (container). For tests of the cgroup setup only.
    #[arg(long, hide = true, default_value = "/sys/fs/cgroup")]
    cgroup_root: PathBuf,
    /// The file naming the daemon's own cgroup (container). For tests of the cgroup
    /// setup only.
    #[arg(long, hide = true, default_value = "/proc/self/cgroup")]
    self_cgroup: PathBuf,
    /// Each container's `pids.max`: its tasks, threads included (container).
    #[arg(long, default_value_t = 8192)]
    container_pids_limit: u64,
    /// The size of each container's `/dev/shm`, in MiB (container).
    #[arg(long, default_value_t = 64)]
    container_shm_mib: u64,
    /// Each container's open-file limit, soft and hard (container). Rootless, it cannot
    /// exceed the daemon's own hard limit.
    #[arg(long, default_value_t = 65_536)]
    container_nofile: u64,
    /// The process limit, soft and hard, of each container's user (container). Every
    /// container's root is the same host id, so it bounds all the node's actions
    /// together.
    #[arg(long, default_value_t = 32_768)]
    container_nproc: u64,
    /// A lease at its own memory cap is killed, and reported out of memory, once it
    /// has pushed more than this many MiB into swap at its cap... (container)
    #[arg(long, default_value_t = 512)]
    lease_swap_kill_mib: u64,
    /// ...or this percentage of its booked memory, whichever is more. The kernel caps
    /// each lease's RAM at its booking x 1.5 + 512 MiB and lets it swap without limit;
    /// a lease is killed by this rule only while its own cap keeps being hit (its
    /// `memory.events` `max` grew since the last sample) and its swap rose since the
    /// last sample, and only swap that rose so counts, never swap that host pressure
    /// moved out of it.
    #[arg(long, default_value_t = 25)]
    lease_swap_kill_percent: u64,
    /// The daemon takes no new lease while the node's memory stall time (the `some`
    /// line's `avg10` in `/proc/pressure/memory`, as a percentage) is above this and
    /// it runs a lease: it says so to the server, which places nothing on it until the
    /// stall falls, and the leases it runs go on. With no lease it serves whatever the
    /// stall, so it is never left idle for good. Off where the kernel reports no
    /// pressure stall information (macOS, a kernel built without it).
    #[arg(long, default_value_t = 10.0, value_parser = percent)]
    memory_pressure_pause_percent: f64,
    /// Where the kernel reports memory pressure stall information. For tests only.
    #[arg(long, hide = true, default_value = kbf_daemon::pressure::MEMORY_PSI)]
    psi_memory_file: PathBuf,
    /// How often each lease's `max` events and swap are sampled, in milliseconds
    /// (container).
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..))]
    lease_swap_poll_ms: u64,
    /// A lease's processes are killed past this percentage of its booked memory...
    #[arg(long, default_value_t = MemoryPolicy::DEFAULT.percent)]
    memory_limit_percent: u64,
    /// ...plus this many MiB (native). A lease that booked nothing has no limit.
    #[arg(long, default_value_t = MemoryPolicy::DEFAULT.headroom_bytes >> 20)]
    memory_headroom_mib: u64,
    /// How often a lease's memory is measured, in milliseconds (native).
    #[arg(long, default_value_t = 250)]
    memory_poll_ms: u64,
    /// The directory searched for `Xcode*.app` (native). Each Xcode whose own
    /// `xcodebuild` answers `-version`, `-license check` and `-checkFirstLaunchStatus`,
    /// and for which `xcrun --find clang` does (each within a minute, under the
    /// actions' sandbox), is ready: it is reported as an `xcode` entry, and an action
    /// that names its build runs with it as `DEVELOPER_DIR`. One that is not is listed
    /// in the node's status with why and the command that fixes it, and logged at WARN.
    /// The daemon says Hello without waiting for its first survey, which runs in the
    /// background: until it ends, each Xcode is listed as not surveyed yet and none is
    /// reported.
    #[arg(long, default_value = xcode::APPLICATIONS)]
    xcode_apps: PathBuf,
    /// How often, in seconds, the Xcodes are asked again (native), so one fixed while
    /// the daemon runs becomes ready without a restart.
    #[arg(long, default_value_t = xcode_watch::EVERY.as_secs())]
    xcode_recheck_secs: u64,
    /// This node's actions use Metal (native): an Xcode whose Metal toolchain is not
    /// installed (`xcodebuild -showComponent MetalToolchain`) is not ready.
    #[arg(long)]
    require_metal_toolchain: bool,
    /// A container image this node is pinned to, as an action names it:
    /// `docker://<repo>@sha256:<digest>`, one architecture's manifest digest
    /// (container; repeatable). At start the daemon checks each is in its user's image
    /// store, as the prepare step of every lease does, and exits with status 2 if one
    /// is not, or names an image index, saying the fix (`podman pull <repo>@sha256:<d>`).
    /// The node's status lists them (`GET /v1/nodes`, `software.container_images`).
    /// No work is placed by image yet: an action may still name another image.
    #[arg(long = "image", value_name = "IMAGE")]
    images: Vec<String>,
    /// The runtime that runs each action's container (container): `podman`, the default,
    /// runs `podman create` and `podman start` per action; `oci` runs each action in a
    /// fresh rootless sandbox through `--oci-runtime` directly, with no Podman on the
    /// action path (Podman only pulls and exports an image once per digest). `oci` needs
    /// the daemon user's subordinate ids in one range, `uidmap`, and every directory down
    /// to `--data-dir` searchable by others.
    #[arg(long, default_value = "podman", value_parser = ["podman", "oci"], value_name = "podman|oci")]
    container_runtime: String,
    /// Where each lease runs among the node's CPUs (container, Linux): `none`, the
    /// default, lets a lease use any CPU the daemon may; `numa-node` puts each lease on
    /// one NUMA node, its CPUs (`cpuset.cpus`) and memory (`cpuset.mems`), the node with
    /// the least CPU booked by running leases (the lowest id on a tie). `--reserve-cpus`
    /// is a quota on all leases together, so it still applies; it removes no CPU from a
    /// node. Needs the `cpuset` controller delegated to the daemon (the start fails and
    /// says how when it is not); a host with fewer than two NUMA nodes that hold CPUs
    /// runs as `none`, which the start logs. Refused with a driver other than
    /// `container` (macOS has no such driver).
    #[arg(long, default_value = "none", value_parser = ["none", "numa-node"], value_name = "none|numa-node")]
    lease_placement: String,
    /// Where the kernel lists NUMA nodes (container). For tests of the placement only.
    #[arg(long, hide = true, default_value = "/sys/devices/system/node")]
    numa_dir: PathBuf,
    /// The OCI runtime `--container-runtime=oci` runs actions with (container).
    #[arg(long, default_value = "/usr/bin/crun")]
    oci_runtime: PathBuf,
    /// A directory holding `passwd`, `subuid` and `subgid` that the container
    /// driver's startup check reads instead of `/etc`'s. For tests of that check only.
    #[arg(long, hide = true)]
    id_files: Option<PathBuf>,
    /// The socket `kbf-mac-session` serves (native-whole-machine).
    #[arg(long, default_value = "/var/run/kbf-mac-session/socket")]
    mac_session_socket: PathBuf,
    /// This daemon's own user, which keeps read and remove access to what a
    /// whole-machine lease's user writes in its lease directory (native-whole-machine).
    #[arg(long, default_value = "_kbf")]
    daemon_user: String,
    /// How many lease users the pool has (native, macOS, a daemon run as root): every
    /// action runs as one of `kbf-pool-0` .. `kbf-pool-<N-1>`, one lease per user at a
    /// time, as kbf-mac-provision makes them (`pool_users`). Required of a root daemon,
    /// which runs no action as root; refused for any other.
    #[arg(long, requires = "pool_first_uid", value_parser = clap::value_parser!(u32).range(1..=64))]
    pool_users: Option<u32>,
    /// The uid of `kbf-pool-0`, and the group id of all the pool users (`pool_first_uid`
    /// in kbf-mac-provision's profile); at least 600.
    #[arg(long, requires = "pool_users")]
    pool_first_uid: Option<u32>,
}

/// The drivers this binary can run leases through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Driver {
    /// Runs nothing; every action succeeds with an empty result. For bring-up only.
    Fake,
    /// Each action in a fresh rootless Podman container (Linux).
    Container,
    /// Each action as plain processes on the node, for Macs.
    Native,
    /// Each whole-machine lease as its own user in its GUI session, through
    /// `kbf-mac-session` (macOS).
    NativeWholeMachine,
}

/// What stops the daemon from starting.
pub type Error = Box<dyn std::error::Error>;

/// The daemon's exit status when an `--image` cannot run on this node.
const IMAGE_UNUSABLE: u8 = 2;

/// An `--image` this node cannot run: missing from the image store, an index, or not
/// an image by digest. The daemon exits with [`IMAGE_UNUSABLE`].
#[derive(Debug)]
pub struct ImageUnusable(String);

impl std::fmt::Display for ImageUnusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "--image: {}", self.0)
    }
}

impl std::error::Error for ImageUnusable {}

/// The exit status for an error that stopped the daemon from starting.
#[must_use]
pub fn exit_code(e: &Error) -> ExitCode {
    if e.is::<ImageUnusable>() {
        ExitCode::from(IMAGE_UNUSABLE)
    } else {
        ExitCode::FAILURE
    }
}

/// Runs the helper this process was started as, if it was: each run's keeper, the
/// lease-user pool's `as-user` step or the container driver's `__chown-dirs`, all of
/// which are this binary re-run with a first argument the daemon's flags never take. A
/// binary calls it first thing, before parsing [`Cli`], and exits with the code when
/// it returns one.
#[must_use]
pub fn dispatch_helpers() -> Option<ExitCode> {
    // Each run's keeper (`kbf-keeper __keeper ...`) is this binary, dispatched before
    // the flags are parsed, so a keeper never reads the daemon's command line.
    if let Some(code) = kbf_daemon::keeper::dispatch() {
        return Some(code);
    }
    // So is the `as-user` step an action of the lease-user pool starts through
    // (`launchctl asuser <uid> kbf-daemon __as-user ...`).
    #[cfg(target_os = "macos")]
    if let Some(code) = kbf_driver_native::slots::dispatch() {
        return Some(code);
    }
    // So is the container driver's `__chown-dirs` helper, which it runs inside Podman's
    // user namespace with the input cache.
    #[cfg(target_os = "linux")]
    if let Some(code) = kbf_driver_container::chown_dirs::dispatch() {
        return Some(ExitCode::from(code));
    }
    None
}

/// The thread sizes the flags and this machine's CPUs give; fails on a reserve the
/// machine cannot give.
/// What the node offers the scheduler: `detected` lowered to what the leases may use.
/// The report sent to the scheduler and the placer's whole-host figure both come from
/// here, so a lease the scheduler books as the whole node is the one the placer leaves
/// unpinned.
fn offered(detected: NodeReport, capacity: Capacity) -> NodeReport {
    detected.within(capacity)
}

fn sizes(cli: &Cli) -> Result<kbf_threads::Sizes, Error> {
    Ok(sizes_on(cli, kbf_threads::Topology::detect())?)
}

/// [`sizes`] on a given machine shape: the reserve, the pools from it, then the
/// overrides.
fn sizes_on(
    cli: &Cli,
    topology: kbf_threads::Topology,
) -> Result<kbf_threads::Sizes, kbf_threads::ReserveError> {
    cli.threads.resolve(topology)
}

/// A daemon set up and not yet serving, with the async runtime the thread flags size.
/// [`NodeHandle::serve`] runs it until a signal; [`NodeHandle::serve_until`] until a
/// future of the caller's.
pub struct NodeHandle {
    tokio: tokio::runtime::Runtime,
    daemon: Daemon,
}

impl NodeHandle {
    /// Serves until SIGTERM or SIGINT.
    ///
    /// # Errors
    /// The signal handlers cannot be installed.
    pub fn serve(self) -> Result<(), Error> {
        // A signal handler is registered with the runtime's reactor, so the runtime's
        // context is entered while it is made.
        let (term, int) = {
            let _context = self.tokio.enter();
            (
                signal(SignalKind::terminate())?,
                signal(SignalKind::interrupt())?,
            )
        };
        self.serve_until(shutdown(term, int));
        Ok(())
    }

    /// Serves until `shutdown` completes, then abandons the leases still running.
    pub fn serve_until(self, shutdown: impl Future<Output = ()>) {
        let Self { tokio, daemon } = self;
        tokio.block_on(daemon.run(shutdown));
    }
}

/// Builds the runtimes `--driver` names and the daemon over them; returns an error
/// that stopped the daemon from starting.
///
/// # Errors
/// The flags name no driver or a combination that cannot run, a driver's start-up
/// checks fail ([`ImageUnusable`] among them, for which [`exit_code`] answers 2), or
/// the node cannot be detected.
pub fn start(cli: &Cli) -> Result<NodeHandle, Error> {
    // The map the frontend pushes lands in `maps`: the daemon fills it, the data plane
    // routes by it.
    let maps = Arc::new(MapHolder::new());
    let held = Arc::clone(&maps);
    start_over(cli, Some(maps), move |cli, liveness| {
        farm(cli, &held, liveness).map(|(plane, _)| plane)
    })
}

/// [`start`], for a daemon in the process of the storage node `local` (`kbf
/// storage-and-daemon` with a frontend): the data plane routes every blob by the map the
/// frontend sends, the blobs of `local` in-process and the others over TLS to their
/// owners ([`kbf_daemon::member_cas`]). There is no `--cas` leg: with no map held a call
/// fails UNAVAILABLE and writes nothing.
///
/// # Errors
/// As [`start`].
pub fn start_member(cli: &Cli, local: LocalMember) -> Result<NodeHandle, Error> {
    let maps = Arc::new(MapHolder::new());
    let held = Arc::clone(&maps);
    start_over(cli, Some(maps), move |cli, liveness| {
        member(cli, &held, &local, liveness)
    })
}

/// [`start`], with every runtime reading and writing blobs through the data plane
/// `plane` makes, rather than through the transfer engine over `--cas`: for a binary
/// that runs the daemon in the server's own process. `plane` is called once, in the
/// async runtime's context, when the first driver that moves blobs is set up (never,
/// with `--driver fake` alone).
///
/// # Errors
/// As [`start`], or what `plane` returns.
pub fn start_with<C: Cas>(
    cli: &Cli,
    plane: impl Fn(&Cli) -> Result<Arc<C>, Error> + 'static,
) -> Result<NodeHandle, Error> {
    start_over(cli, None, move |cli, _| plane(cli))
}

/// [`start_with`], and the daemon keeping the storage map its frontend pushes in `maps`
/// (`None`: the daemon ignores the map, as a server's own process does).
fn start_over<C: Cas>(
    cli: &Cli,
    maps: Option<Arc<MapHolder>>,
    plane: impl Fn(&Cli, Liveness) -> Result<Arc<C>, Error> + 'static,
) -> Result<NodeHandle, Error> {
    // Refused here, before any driver is set up, though only the plane over a storage
    // map reads the values; the plane gets the checked values, not the flags again.
    let liveness = cli.daemon.storage_liveness()?;
    let sizes = sizes(cli)?;
    sizes.log();
    let tokio = sizes.runtime()?;
    let daemon = {
        // The CAS channel is made lazily, which needs the runtime's context.
        let _context = tokio.enter();
        let xcrun = Path::new(xcode::XCRUN);
        // The update client is the frontend's daemon's alone: the in-process daemon of
        // the all-in-one binary keeps none (its flags may not name one, so that none is
        // silently ignored).
        let update = match &maps {
            Some(_) => Some(update::link(cli)?),
            None if cli.update_dir.is_some() => {
                return Err("--update-dir is not taken by the in-process daemon yet".into());
            }
            None => None,
        };
        let (daemon, _) = node_over(
            cli,
            || native_config(cli),
            xcrun,
            maps,
            Box::new(move |cli| plane(cli, liveness)),
        )?;
        let daemon = update.into_iter().fold(daemon, Daemon::with_update);
        backstop(cli, daemon)
    };
    Ok(NodeHandle { tokio, daemon })
}

/// The data plane of a daemon whose server is elsewhere: the process's transfer engine
/// ([`transfer::engine`]) over `--cas`, used until the frontend sends a storage map and
/// not after (`kbf_daemon::FarmCas`). With a map every blob goes to the storage node that
/// owns it, verified against its own name from the map (the daemon's CA and certificate,
/// never `--tls-server-name`, which names the frontend). A frontend that sends no map
/// leaves the daemon on `--cas`, as before the map existed. Each change of side is
/// logged once, by the plane.
///
/// Answers the plane with the liveness its storage nodes hold, read back from them: the
/// plane keeps them as `dyn Nodes`, which has none to read, and a test must see that the
/// flags arrived (`start` drops it).
fn farm(
    cli: &Cli,
    maps: &Arc<MapHolder>,
    liveness: Liveness,
) -> Result<(Arc<FarmCas>, Liveness), Error> {
    let server = Arc::new(transfer::engine(cli)?);
    let nodes = kbf_daemon::storage_nodes(&DaemonConfig::from_args(&cli.daemon).tls, liveness)?;
    tracing::info!(
        "data plane: through --cas until the frontend sends a storage map, then to the \
         storage nodes it names"
    );
    let held = nodes.liveness();
    let plane = FarmCas::new(server, Arc::clone(maps), nodes, &cli.transfer.config());
    Ok((Arc::new(plane), held))
}

/// The data plane of [`start_member`]: the daemon's CA and certificate for the members
/// that are not `local`, each verified against its own name from the map.
fn member(
    cli: &Cli,
    maps: &Arc<MapHolder>,
    local: &LocalMember,
    liveness: Liveness,
) -> Result<Arc<kbf_transfer::RoutedCas>, Error> {
    let nodes = kbf_daemon::storage_nodes(&DaemonConfig::from_args(&cli.daemon).tls, liveness)?;
    Ok(kbf_daemon::member_cas(
        Arc::clone(maps),
        local.clone(),
        nodes,
        &cli.transfer.config(),
    ))
}

/// The engine over `--cas` alone, for tests of the node's set-up.
#[cfg(test)]
fn remote(cli: &Cli) -> Result<Arc<kbf_transfer::Transfers>, Error> {
    Ok(Arc::new(transfer::engine(cli)?))
}

/// `daemon` with the memory backstop `--memory-pressure-pause-percent` sets, if the
/// node reports memory pressure ([`memory_stall`]). Needs a tokio runtime's context.
fn backstop(cli: &Cli, daemon: Daemon) -> Daemon {
    match memory_stall(cli) {
        Some(stall) => {
            tracing::info!(
                above_percent = cli.memory_pressure_pause_percent,
                "memory backstop: on"
            );
            daemon.with_memory_backstop(stall, cli.memory_pressure_pause_percent)
        }
        None => daemon,
    }
}

/// A percentage above 0 and at most 100, for `--memory-pressure-pause-percent`.
fn percent(text: &str) -> Result<f64, String> {
    let value: f64 = text
        .parse()
        .map_err(|e| format!("{text:?} is not a number: {e}"))?;
    if value > 0.0 && value <= 100.0 {
        Ok(value)
    } else {
        Err(format!("{text} is not above 0 and at most 100"))
    }
}

/// The node's memory stall time, sampled every [`kbf_daemon::pressure::SAMPLE_EVERY`]
/// from the file `--psi-memory-file` names; `None` (the backstop off, logged) when the
/// file cannot be read now, as on a kernel without pressure stall information. Needs a
/// tokio runtime's context.
fn memory_stall(cli: &Cli) -> Option<tokio::sync::watch::Receiver<Option<f64>>> {
    let stall = kbf_daemon::pressure::watch(
        cli.psi_memory_file.clone(),
        kbf_daemon::pressure::SAMPLE_EVERY,
    );
    if stall.borrow().is_none() {
        tracing::info!(
            file = %cli.psi_memory_file.display(),
            "memory backstop: off: the file cannot be read"
        );
        return None;
    }
    Some(stall)
}

/// The drivers `--driver` names, each once: the node's own (native or container)
/// first, then the others in the order given.
fn drivers(cli: &Cli) -> Vec<Driver> {
    let own = |d: &Driver| matches!(d, Driver::Native | Driver::Container);
    let ordered = cli.drivers.iter().filter(|d| own(d));
    let mut drivers: Vec<Driver> = Vec::new();
    for driver in ordered.chain(cli.drivers.iter().filter(|d| !own(d))) {
        if !drivers.contains(driver) {
            drivers.push(*driver);
        }
    }
    drivers
}

/// What the drivers set up so far give the daemon, whose runtimes move blobs through
/// the data plane `C`.
struct Node<C> {
    /// One runtime per driver; `None` before the first.
    runtimes: Option<Runtimes>,
    /// What leases may use (the container driver's `actions/` cgroup).
    capacity: Capacity,
    /// The native driver's Xcode reports.
    reports: Option<Reports>,
    /// The whole-machine runtime's phase.
    phase: Option<tokio::sync::watch::Receiver<kbf_types::NodePhase>>,
    /// The data plane every runtime shares, made by the first that needs it.
    plane: Plane<C>,
}

/// A [`Node`]'s data plane: how to make it until a runtime asks for it, then it.
enum Plane<C> {
    Unmade(MakePlane<C>),
    Made(Arc<C>),
}

/// What makes a daemon's data plane from its flags.
type MakePlane<C> = Box<dyn Fn(&Cli) -> Result<Arc<C>, Error>>;

impl<C: Cas> Node<C> {
    /// No runtime yet, its data plane made by `plane` when a runtime first asks.
    fn new(plane: MakePlane<C>) -> Self {
        Self {
            runtimes: None,
            capacity: Capacity::default(),
            reports: None,
            phase: None,
            plane: Plane::Unmade(plane),
        }
    }

    /// The process's data plane, made on the first call; every later call returns the
    /// same one.
    fn plane(&mut self, cli: &Cli) -> Result<Arc<C>, Error> {
        let made = match &self.plane {
            Plane::Made(made) => return Ok(Arc::clone(made)),
            Plane::Unmade(make) => make(cli)?,
        };
        self.plane = Plane::Made(Arc::clone(&made));
        Ok(made)
    }

    /// Adds `runtime`. Fails when it serves a lease kind a runtime added before serves.
    fn add<R: Runtime>(&mut self, runtime: Arc<R>) -> Result<(), Error> {
        self.runtimes = Some(match self.runtimes.take() {
            None => Runtimes::new(runtime),
            Some(set) => set.and(runtime)?,
        });
        Ok(())
    }
}

/// The daemon with a runtime for each of [`drivers`], set up in that order, every one
/// moving blobs through the one data plane `plane` makes; the native driver with the
/// configuration `native` makes and the `xcrun` it surveys with and
/// warms (a test names its own user folders and sandbox, and an `xcrun` of its own).
/// Also returns what the native driver's Xcode watch sends the daemon, for a test to
/// follow. Its node report is the detected entries, the labels and a `drivers` entry
/// per runtime (which `Daemon::new` adds), its `cpus` and `mem_gib` no more than the
/// container driver allows; the native driver adds its own with
/// `Daemon::with_driver_report`. Its status lists every `--image`, which only the
/// container driver runs: without it, `--image` fails the start. With a real driver, once
/// every runtime is built, it claims the lease directory and names the claim it found
/// as its predecessor (`instance`). With `maps`, the daemon keeps the storage map its
/// frontend pushes there.
fn node_over<C: Cas>(
    cli: &Cli,
    native: impl Fn() -> Result<NativeConfig, Error>,
    xcrun: &Path,
    maps: Option<Arc<MapHolder>>,
    plane: MakePlane<C>,
) -> Result<(Daemon, Option<Reports>), Error> {
    if !cli.images.is_empty() && !cli.drivers.contains(&Driver::Container) {
        return Err("--image names container images: only --driver container runs them".into());
    }
    let plan = data_dir(cli)?;
    if cli.lease_placement != "none" && !cli.drivers.contains(&Driver::Container) {
        return Err(format!(
            "--lease-placement={} places container leases on NUMA nodes: only --driver container \
             runs them (Linux only; macOS has no NUMA nodes or cpusets)",
            cli.lease_placement
        )
        .into());
    }
    let mut node = Node::new(plane);
    for driver in drivers(cli) {
        match driver {
            Driver::Fake => node.add(Arc::new(FakeRuntime::new(Duration::ZERO)))?,
            Driver::Native => {
                #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
                let mut config = native()?;
                // Its leases' inputs are clones of the data directory's cache and trees
                // on macOS; elsewhere the plan makes neither.
                #[cfg(target_os = "macos")]
                if let Some(plan) = plan.as_ref().filter(|plan| plan.cached) {
                    config.cache = data_dir::native_cache(plan);
                    config.trees = data_dir::native_trees(plan, config.cache.as_ref());
                }
                let cas = node.plane(cli)?;
                let (runtime, reports) = native_runtime(cli, config, xcrun, cas)?;
                node.add(runtime)?;
                node.reports = Some(reports);
            }
            Driver::Container => container::add(cli, &mut node, &plan)?,
            Driver::NativeWholeMachine => whole_machine::add(cli, &mut node)?,
        }
    }
    let runtimes = node.runtimes.ok_or("no --driver given")?;
    // Whatever the driver, leases get the CPUs the daemon may use but the reserve (the
    // container driver's `actions/cpu.max` says the same to the kernel).
    let leases = sizes(cli)?.lease_cpus() as u64;
    node.capacity.cpus = Some(node.capacity.cpus.map_or(leases, |cpus| cpus.min(leases)));
    // `Daemon::new` adds every runtime's driver.
    let report = offered(
        NodeReport::detect(&[])?.with_entries(cli.daemon.label_entries()),
        node.capacity,
    );
    // A node without the whole-machine runtime is always serving: a phase that never
    // changes.
    let phase = node
        .phase
        .unwrap_or_else(|| tokio::sync::watch::channel(kbf_types::NodePhase::Serving).1);
    let daemon = Daemon::new(DaemonConfig::from_args(&cli.daemon), runtimes, report)?
        .with_container_images(cli.images.iter().cloned())
        .with_phase(phase);
    // One line, not a match: this function has an instance per data plane, and an arm
    // each instance never takes would be a line the coverage ratchet counts as missed.
    let daemon = maps.into_iter().fold(daemon, Daemon::with_map);
    // Every runtime is built, so every sweep has run: claim the scratch directory.
    let daemon = match &plan {
        Some(plan) => instance::named(daemon, &plan.leases()),
        None => daemon,
    };
    Ok(match node.reports {
        Some(reports) => (daemon.with_driver_report(reports.clone()), Some(reports)),
        None => (daemon, None),
    })
}

/// [`node_over`] with the transfer engine over `--cas` as the data plane.
#[cfg(test)]
fn node(
    cli: &Cli,
    native: impl Fn() -> Result<NativeConfig, Error>,
    xcrun: &Path,
) -> Result<(Daemon, Option<Reports>), Error> {
    node_over(cli, native, xcrun, None, Box::new(remote))
}

/// [`node`] with the native driver alone, its configuration `config`.
#[cfg(test)]
fn native_with(cli: &Cli, config: NativeConfig, xcrun: &Path) -> Result<(Daemon, Reports), Error> {
    let (daemon, reports) = node(cli, || Ok(config.clone()), xcrun)?;
    Ok((daemon, reports.ok_or("no native driver")?))
}

/// The native driver with `config`, the `xcrun` it surveys with and warms, and the
/// process's data plane `cas`. It
/// surveys the Xcodes in `--xcode-apps` in the background, at once and again every
/// `--xcode-recheck-secs`, and returns each changed survey for the daemon
/// (`Daemon::with_driver_report`): without it the node reports no Xcode, ready or
/// not. It does not wait for the first survey: until that ends, the daemon reports
/// every Xcode found as not surveyed yet, and none as ready.
fn native_runtime<C: Cas>(
    cli: &Cli,
    mut config: NativeConfig,
    xcrun: &Path,
    cas: Arc<C>,
) -> Result<(Arc<NativeRuntime<C>>, Reports), Error> {
    // The simulators of the leases that ask for one are made with the same `xcrun`.
    config.xcrun = xcrun.to_owned();
    let runtime = NativeRuntime::new(config, cas)?;
    let runtime = Arc::new(runtime);
    let watched = Arc::clone(&runtime);
    let (mut probe, every) = xcode_watch_args(cli);
    probe.xcrun = xcrun.to_owned();
    // Every question runs as an action does: xcrun reads and fills its cache, which
    // leases can write, only under the sandbox.
    probe.sandbox = Some(runtime.sandbox(kbf_driver_native::SURVEY_DIR));
    let warm = xcrun.to_owned();
    let warmed = std::sync::Once::new();
    let apply = move |xcodes: &[xcode::Xcode]| {
        let report = watched.apply_xcodes(xcodes);
        // Once the first survey is applied, in the background and sandboxed as an
        // action: xcrun fills its cache for the node's own Xcode and the ready ones
        // while the node serves; each later survey warms the Xcodes it makes ready.
        // Not before: the warm-up's lookups would make the survey's miss.
        if xcodes.iter().all(|x| x.state != xcode::State::NotSurveyed) {
            warmed.call_once(|| {
                let _ = watched.warm_xcrun(&warm, xcode::ANSWER_WITHIN);
            });
        }
        report
    };
    // One part per survey, so none overwrites another's entries; the Xcodes' is the
    // only one yet.
    let (driver, reports) = DriverWatch::new();
    let xcodes = driver
        .part(xcode_watch::SOURCE)
        .ok_or("the Xcode part of the driver watch is taken")?;
    // Returns at once: the survey runs on the watch's thread.
    let _ = xcode_watch::watch(
        xcodes,
        cli.xcode_apps.clone(),
        probe,
        every,
        Box::new(apply),
    );
    Ok((runtime, reports))
}

/// What the native driver's surveys send the daemon, merged.
type Reports = tokio::sync::watch::Receiver<kbf_daemon::DriverReport>;

/// Completes on SIGTERM or SIGINT.
async fn shutdown(mut term: Signal, mut int: Signal) {
    let name = tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
    };
    tracing::info!("{name}: shutting down");
}

/// The data directory, resolved and made, for a daemon that runs a driver that keeps
/// lease directories (every one but `fake`) or is given `--data-dir`; `None` for the
/// fake driver alone without it, which touches no directory. Logs every directory and
/// budget it resolved.
fn data_dir(cli: &Cli) -> Result<Option<data_dir::Plan>, Error> {
    let real = |d: &Driver| *d != Driver::Fake;
    if !cli.drivers.iter().any(real) && cli.data.data_dir.is_none() {
        return Ok(None);
    }
    // The drivers that link or clone leases' inputs from the cache.
    let cached = cli.drivers.contains(&Driver::Container)
        || (cfg!(target_os = "macos") && cli.drivers.contains(&Driver::Native));
    // The OCI backend's container ids are not the daemon's: `leases` must be searchable.
    let oci = cli.drivers.contains(&Driver::Container) && cli.container_runtime == "oci";
    let plan = data_dir::Plan::new(&cli.data, cached, &Statvfs)?.with_open_leases(oci);
    plan.create()?;
    for line in plan.describe() {
        tracing::info!("{line}");
    }
    Ok(Some(plan))
}

/// The lease directory root, `<--data-dir>/leases`, which the real drivers require.
fn scratch(cli: &Cli) -> Result<PathBuf, Error> {
    let dir = cli.data.leases();
    if !dir.is_absolute() {
        return Err(format!("--data-dir {} must be absolute", cli.data.dir().display()).into());
    }
    Ok(dir)
}

/// `--output-upload-deadline-secs`.
fn upload_deadline(cli: &Cli) -> Duration {
    Duration::from_secs(cli.output_upload_deadline_secs)
}

fn native_config(cli: &Cli) -> Result<NativeConfig, Error> {
    let mut config = NativeConfig::new(scratch(cli)?);
    config.outputs = cli.outputs;
    config.upload_deadline = upload_deadline(cli);
    config.memory = MemoryPolicy {
        percent: cli.memory_limit_percent,
        headroom_bytes: cli.memory_headroom_mib.saturating_mul(1 << 20),
    };
    config.poll = Duration::from_millis(cli.memory_poll_ms.max(1));
    config.pool = (cli.pool_users.zip(cli.pool_first_uid))
        .map(|(count, first)| Pool::provisioned(count, first))
        .transpose()?;
    Ok(config)
}

/// How the native driver asks its Xcodes (the system's tools, and the Metal toolchain
/// if required), and how often it asks again (at least every second).
fn xcode_watch_args(cli: &Cli) -> (xcode::Probe, Duration) {
    let probe = xcode::Probe::system(cli.require_metal_toolchain);
    (probe, Duration::from_secs(cli.xcode_recheck_secs.max(1)))
}

#[cfg(target_os = "linux")]
mod container {
    use std::sync::Arc;

    use std::time::Duration;

    use kbf_driver_container::{
        ContainerLimits, IdFiles, ImageRef, LeasePlacement, OutputLimits, PodmanConfig,
        PodmanRuntime, SwapKill,
    };

    use super::{Cli, Error, ImageUnusable, Node, scratch};
    use crate::data_dir::Plan as DataPlan;
    use kbf_transfer::Cas;

    /// Builds the container driver, over the daemon's data plane, and adds it to `node`
    /// with what its leases may use.
    pub(super) fn add<C: Cas>(
        cli: &Cli,
        node: &mut Node<C>,
        plan: &Option<DataPlan>,
    ) -> Result<(), Error> {
        // The checks that change nothing come first.
        let scratch = scratch(cli)?;
        let cas = node.plane(cli)?;
        // Every container's ids are this user's subordinate ids (`--userns=nomap`).
        let files = cli
            .id_files
            .as_deref()
            .map_or_else(IdFiles::system, IdFiles::in_dir);
        kbf_driver_container::check_daemon_user(&files)?;
        let mount = &cli.cgroup_root;
        let memory_max = cli
            .actions_memory_max_gib
            .map(|gib| gib.saturating_mul(1 << 30));
        let parent = match &cli.cgroup_parent {
            Some(parent) => {
                kbf_driver_container::adopt(mount, parent, memory_max)?;
                parent.clone()
            }
            None => {
                let own = std::fs::read_to_string(&cli.self_cgroup)
                    .map_err(|e| format!("read {}: {e}", cli.self_cgroup.display()))?;
                let memory_min = cli.supervisor_memory_min_mib.saturating_mul(1 << 20);
                let delegation =
                    kbf_driver_container::delegate(mount, &own, memory_min, memory_max)?;
                tracing::info!(
                    "cgroup {}: this daemon in {}/supervisor (memory.min {} MiB), leases under {}",
                    delegation.root,
                    delegation.root,
                    cli.supervisor_memory_min_mib,
                    delegation.actions
                );
                if let Some((cgroup, min)) = &delegation.memory_min_capped {
                    tracing::warn!(
                        "cgroup {cgroup} has memory.min {min} bytes, below the daemon's {memory_min}: \
                         the kernel protects the daemon no further; set MemoryMin= on its unit \
                         and slice to at least {} MiB",
                        cli.supervisor_memory_min_mib
                    );
                }
                delegation.actions
            }
        };
        // The leases' share of the CPUs: all the daemon may use but `--reserve-cpus`.
        // A limit on the actions cgroup, not a pinning: any CPU, at most this many.
        kbf_driver_container::limit_cpus(mount, &parent, super::sizes(cli)?.lease_cpus() as u64)?;
        let capacity = kbf_driver_container::capacity(mount, &parent)?;
        tracing::info!(
            "leases may use {} CPUs and {} of memory (cgroup {parent})",
            capacity.cpus.map_or("all".to_owned(), |n| n.to_string()),
            capacity
                .memory_bytes
                .map_or("all".to_owned(), |b| format!("{} GiB", b >> 30))
        );
        // After the cgroup setup it builds on, before the runtime sweeps and serves.
        let placer = match cli.lease_placement.parse()? {
            LeasePlacement::None => None,
            LeasePlacement::NumaNode => {
                // The memory the scheduler books on this node (the report's `mem_gib`):
                // a lease that asks for all of it is the whole host's and is not pinned.
                let whole_host =
                    super::offered(kbf_daemon::NodeReport::detect(&[])?, capacity).mem_bytes();
                kbf_driver_container::numa::start(&cli.numa_dir, mount, &parent, whole_host)?
            }
        };
        let mut config = PodmanConfig::new(scratch, parent, cli.daemon.node_id.clone());
        config.placer = placer;
        config.container_runtime = cli.container_runtime.parse()?;
        config.oci.runtime = cli.oci_runtime.clone();
        config.oci.id_files = files;
        config.cgroup_root = mount.clone();
        config.outputs = OutputLimits {
            max_depth: cli.outputs.max_depth,
            max_entries: cli.outputs.max_entries,
            max_bytes: cli.outputs.max_bytes,
            max_stdio_bytes: cli.outputs.max_stdio_bytes,
        };
        config.upload_deadline = super::upload_deadline(cli);
        config.cpu = cpu_pool(cli)?;
        config.limits = limits(cli);
        config.swap_kill = swap_kill(cli);
        config.cache = crate::data_dir::given(plan.as_ref(), &cli.data);
        let runtime = PodmanRuntime::new(config, cas)?;
        // From `start`, in the runtime's context but outside any task.
        tokio::runtime::Handle::current().block_on(check_images(&runtime, &cli.images))?;
        node.add(Arc::new(runtime))?;
        node.capacity = capacity;
        Ok(())
    }

    /// Checks each `--image` as a lease's prepare step would: named by digest, in this
    /// node's image store, one architecture's manifest. The first that is not stops the
    /// start, with its fix.
    async fn check_images<C: Cas>(
        runtime: &PodmanRuntime<C>,
        images: &[String],
    ) -> Result<(), ImageUnusable> {
        for value in images {
            let image = ImageRef::parse(value).map_err(|e| ImageUnusable(e.to_string()))?;
            runtime
                .check_pinned(&image)
                .await
                .map_err(|e| ImageUnusable(e.to_string()))?;
            tracing::info!("image {image}: in this node's image store");
        }
        Ok(())
    }

    /// The `--lease-swap-*` flags.
    pub(super) fn swap_kill(cli: &Cli) -> SwapKill {
        SwapKill {
            floor_bytes: cli.lease_swap_kill_mib.saturating_mul(1 << 20),
            percent: cli.lease_swap_kill_percent,
            every: Duration::from_millis(cli.lease_swap_poll_ms),
        }
    }

    /// The CPU pool the container driver hashes outputs on, sized by `--cpu-threads`.
    pub(super) fn cpu_pool(cli: &Cli) -> Result<kbf_daemon::CpuPool, Error> {
        Ok(kbf_daemon::CpuPool::new(super::sizes(cli)?.cpu_threads()))
    }

    /// The `--container-*` limits.
    pub(super) fn limits(cli: &Cli) -> ContainerLimits {
        ContainerLimits {
            pids: cli.container_pids_limit,
            shm_mib: cli.container_shm_mib,
            nofile: cli.container_nofile,
            nproc: cli.container_nproc,
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod container {
    use super::{Cli, Error, Node};
    use crate::data_dir::Plan;

    pub(super) fn add<C>(
        _cli: &Cli,
        _node: &mut Node<C>,
        _plan: &Option<Plan>,
    ) -> Result<(), Error> {
        Err("the container driver runs on Linux only; use --driver native".into())
    }
}

mod data_dir;
mod instance;
mod run;
mod transfer;
mod update;
mod whole_machine;

#[cfg(test)]
mod deploy_doc_tests;
#[cfg(test)]
mod instance_tests;
#[cfg(test)]
mod startup_tests;

#[cfg(test)]
mod tests {
    use clap::Parser;
    use kbf_daemon::DriverReport;
    use kbf_proto::worker::XcodeState;

    use super::*;

    /// The first report the watch sends with no Xcode left not surveyed, waited for up
    /// to a minute.
    pub(crate) async fn surveyed(reports: &mut Reports) -> DriverReport {
        let pending = |r: &DriverReport| {
            r.xcodes
                .iter()
                .any(|x| x.state() == XcodeState::NotSurveyed)
        };
        let surveyed = reports.wait_for(|r| !pending(r));
        tokio::time::timeout(Duration::from_secs(60), surveyed)
            .await
            .expect("the first survey ends in time")
            .expect("the watch runs")
            .clone()
    }

    /// With the host's memory above the actions cgroup's cap, the placer's whole-host
    /// figure is the capped memory the scheduler sees, not the host's.
    #[test]
    fn the_placers_whole_host_is_the_memory_the_scheduler_sees() {
        let detected = || NodeReport::new([("mem_gib", "251"), ("cpus", "64")]);
        let cap = Capacity {
            cpus: None,
            memory_bytes: Some(175 << 30),
        };
        let reported = offered(detected().with_entries([("zone", "a")]), cap);
        let whole_host = offered(detected(), cap).mem_bytes();
        assert_eq!(whole_host, Some(175 << 30));
        assert_eq!(whole_host, reported.mem_bytes());
    }

    fn parse(extra: &[&str]) -> Result<Cli, clap::Error> {
        let base = [
            "kbf-daemon",
            "--server=https://front:7070",
            "--ca-cert=ca.pem",
            "--cert=c.pem",
            "--key=c.key",
            "--node-id=mac-1",
        ];
        Cli::try_parse_from(base.iter().chain(extra))
    }

    /// Catches: native flags that do not reach the driver's configuration (the
    /// memory limit, poll and output limits, where Xcodes are looked for, how often
    /// they are asked again, whether the Metal toolchain is required), a re-check
    /// every 0 s (a busy loop), and a relative or missing scratch directory accepted.
    #[test]
    fn native_flags_reach_the_configuration() {
        let cli = parse(&[
            "--driver=native",
            "--data-dir=/var/kbf",
            "--memory-limit-percent=200",
            "--memory-headroom-mib=64",
            "--memory-poll-ms=0",
            "--output-max-bytes=99",
            "--output-upload-deadline-secs=7",
            "--xcode-apps=/var/kbf/apps",
        ])
        .expect("flags");
        assert_eq!(cli.drivers, [Driver::Native]);
        let config = native_config(&cli).expect("config");
        assert_eq!(config.scratch, PathBuf::from("/var/kbf/leases"));
        assert_eq!(config.memory.limit(1 << 20), Some((2 << 20) + (64 << 20)));
        assert_eq!(config.poll, Duration::from_millis(1));
        assert_eq!(config.outputs.max_bytes, 99);
        assert_eq!(config.upload_deadline, Duration::from_secs(7));
        assert_eq!(cli.xcode_apps, PathBuf::from("/var/kbf/apps"));
        assert_eq!(
            xcode_watch_args(&cli),
            (xcode::Probe::system(false), xcode_watch::EVERY)
        );
        let metal = parse(&[
            "--driver=native",
            "--require-metal-toolchain",
            "--xcode-recheck-secs=0",
        ])
        .expect("flags");
        assert_eq!(
            xcode_watch_args(&metal),
            (xcode::Probe::system(true), Duration::from_secs(1))
        );
        let defaults = parse(&["--driver=native", "--data-dir=/s"]).expect("flags");
        assert_eq!(
            native_config(&defaults).expect("config").memory,
            MemoryPolicy::DEFAULT
        );
        assert_eq!(defaults.xcode_apps, PathBuf::from("/Applications"));
        let relative = parse(&["--driver=native", "--data-dir=leases"]).expect("flags");
        assert!(native_config(&relative).is_err());
        let missing = parse(&["--driver=native"]).expect("flags");
        assert_eq!(
            native_config(&missing).expect("config").scratch,
            PathBuf::from("/var/lib/kbf/leases"),
            "the default data directory"
        );
        assert_eq!(native_config(&defaults).expect("config").pool, None);
    }

    /// Catches: a default other than 10 percent, a value that does not reach the
    /// field, and a threshold the daemon cannot use accepted (0 or less pauses a node
    /// on any stall at all or never; over 100 never pauses; a word or NaN is no
    /// percentage).
    #[test]
    fn the_memory_pressure_threshold_is_a_percentage() {
        let pause = |args: &[&str]| parse(args).map(|cli| cli.memory_pressure_pause_percent);
        assert_eq!(pause(&["--driver=fake"]).expect("flags"), 10.0);
        assert_eq!(
            pause(&["--driver=fake", "--memory-pressure-pause-percent=2.5"]).expect("flags"),
            2.5
        );
        assert_eq!(
            pause(&["--driver=fake", "--memory-pressure-pause-percent=100"]).expect("flags"),
            100.0
        );
        for bad in ["0", "-1", "100.5", "lots", "NaN", ""] {
            let flag = format!("--memory-pressure-pause-percent={bad}");
            assert!(pause(&["--driver=fake", &flag]).is_err(), "{bad:?} parsed");
        }
    }

    /// Catches a backstop that is attached without a readable pressure file (a node
    /// with no pressure stall information would be paused or crash), one that is not
    /// attached with one (a Linux node would never stop taking leases), one attached
    /// with another threshold than the flag's, and a sampler that does not read the
    /// file it is given.
    #[tokio::test]
    async fn the_backstop_is_attached_only_where_the_kernel_reports_pressure() {
        let dir = scratch("psi");
        tls_files(&dir);
        let file = dir.join("memory");
        std::fs::write(&file, "some avg10=1.50 avg60=0.00 avg300=0.00 total=1\n").expect("write");
        let cli = |psi: &Path| {
            let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
            Cli::try_parse_from([
                "kbf-daemon".to_owned(),
                "--server=https://127.0.0.1:1".to_owned(),
                flag("ca-cert", &dir.join("ca.pem")),
                flag("cert", &dir.join("node.pem")),
                flag("key", &dir.join("node.key")),
                "--node-id=mac-1".to_owned(),
                "--driver=fake".to_owned(),
                "--memory-pressure-pause-percent=7.5".to_owned(),
                flag("psi-memory-file", psi),
            ])
            .expect("flags")
        };
        let attached = |cli: &Cli| {
            let report = NodeReport::detect(&[]).expect("a report");
            let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
            let config = DaemonConfig::from_args(&cli.daemon);
            let daemon = Daemon::new(config, runtime, report).expect("a daemon");
            backstop(cli, daemon).memory_backstop()
        };
        let on = cli(&file);
        assert_eq!(attached(&on), Some(7.5));
        assert_eq!(*memory_stall(&on).expect("readable").borrow(), Some(1.5));
        let off = cli(&dir.join("missing"));
        assert_eq!(attached(&off), None);
        assert!(memory_stall(&off).is_none());
    }

    /// Catches pool flags that do not reach the driver (a root daemon would refuse to
    /// start, or run every action as one user), a pool of other users than
    /// kbf-mac-provision makes, one flag without the other taken, and a pool at root's
    /// uid accepted.
    #[test]
    fn the_pool_flags_reach_the_configuration() {
        let pooled = |extra: &[&str]| {
            let flags = [&["--driver=native", "--data-dir=/s"][..], extra].concat();
            parse(&flags)
        };
        let cli = pooled(&["--pool-users=2", "--pool-first-uid=700"]).expect("flags");
        let pool = native_config(&cli).expect("config").pool.expect("a pool");
        assert_eq!(Some(pool), Pool::provisioned(2, 700).ok());
        for one in ["--pool-users=2", "--pool-first-uid=700"] {
            assert!(pooled(&[one]).is_err(), "{one} alone");
        }
        for count in ["0", "65"] {
            assert!(pooled(&[&format!("--pool-users={count}"), "--pool-first-uid=700"]).is_err());
        }
        let root = pooled(&["--pool-users=1", "--pool-first-uid=0"]).expect("flags");
        let refused = native_config(&root).expect_err("a pool at uid 0");
        assert!(refused.to_string().contains("root"), "{refused}");
    }

    /// Catches an upload deadline that defaults to anything but
    /// `kbf_daemon::UPLOAD_DEADLINE` (30 minutes), and a deadline of 0 accepted (every
    /// lease with outputs would fail).
    #[test]
    fn the_upload_deadline_defaults_to_thirty_minutes_and_is_never_zero() {
        let cli = parse(&["--driver=native"]).expect("flags");
        assert_eq!(upload_deadline(&cli), Duration::from_secs(1800));
        assert_eq!(upload_deadline(&cli), kbf_daemon::UPLOAD_DEADLINE);
        assert!(parse(&["--driver=native", "--output-upload-deadline-secs=0"]).is_err());
    }

    /// Catches a `--container-*` flag that does not reach the driver's limits, and a
    /// flag default that differs from the driver's documented
    /// `ContainerLimits::DEFAULT`.
    #[cfg(target_os = "linux")]
    #[test]
    fn container_limit_flags_reach_the_configuration() {
        use kbf_driver_container::ContainerLimits;
        let cli = parse(&[
            "--driver=container",
            "--container-pids-limit=11",
            "--container-shm-mib=12",
            "--container-nofile=13",
            "--container-nproc=14",
        ])
        .expect("flags");
        assert_eq!(
            container::limits(&cli),
            ContainerLimits {
                pids: 11,
                shm_mib: 12,
                nofile: 13,
                nproc: 14,
            }
        );
        let defaults = parse(&["--driver=container"]).expect("flags");
        assert_eq!(container::limits(&defaults), ContainerLimits::DEFAULT);
    }

    /// Catches `--cpu-threads` not parsed, not reaching the sizes the CPU pool is built
    /// from, or accepting 0 (a pool that never runs a job).
    #[test]
    fn cpu_threads_is_a_flag() {
        let cli = parse(&["--driver=fake", "--cpu-threads=6"]).expect("flags");
        assert_eq!(sizes(&cli).expect("sizes").cpu_threads(), 6);
        let cli = parse(&["--driver=fake"]).expect("flags");
        assert!(sizes(&cli).expect("sizes").cpu_threads() >= 1);
        assert!(parse(&["--driver=fake", "--cpu-threads=0"]).is_err());
    }

    /// Catches the container driver's pool not following `--cpu-threads` (a fixed size
    /// in its construction). 6 and 3 are not the default on any machine's topology.
    #[cfg(target_os = "linux")]
    #[test]
    fn cpu_threads_reaches_the_container_pool() {
        let cli = parse(&["--driver=container", "--cpu-threads=6"]).expect("flags");
        assert_eq!(container::cpu_pool(&cli).expect("pool").threads(), 6);
        let cli = parse(&["--driver=container", "--cpu-threads=3"]).expect("flags");
        assert_eq!(container::cpu_pool(&cli).expect("pool").threads(), 3);
    }

    /// Catches the daemon resolving another role's defaults: on the 44-core,
    /// 2-threads-per-core farm node it is a reserve of 8, so 2 async, 64 blocking and 4
    /// CPU threads and 80 CPUs for leases (the server's would be 22, 128, 44), a
    /// flag still wins over its default, the pools follow `--reserve-cpus`, and a
    /// reserve of the whole machine is refused.
    #[test]
    fn the_daemon_resolves_the_daemon_role() {
        let topology = || kbf_threads::Topology::new(88, 2);
        let cli = parse(&["--driver=fake"]).expect("flags");
        let sizes = sizes_on(&cli, topology()).expect("sizes");
        assert_eq!(
            (
                sizes.async_threads(),
                sizes.blocking_threads(),
                sizes.cpu_threads(),
                sizes.lease_cpus()
            ),
            (2, 64, 4, 80)
        );
        let cli = parse(&["--driver=fake", "--async-threads=5"]).expect("flags");
        assert_eq!(
            sizes_on(&cli, topology()).expect("sizes").async_threads(),
            5
        );
        let cli = parse(&["--driver=fake", "--reserve-cpus=40"]).expect("flags");
        let sizes = sizes_on(&cli, topology()).expect("sizes");
        assert_eq!(
            (
                sizes.async_threads(),
                sizes.cpu_threads(),
                sizes.lease_cpus()
            ),
            (10, 20, 48)
        );
        let cli = parse(&["--driver=fake", "--reserve-cpus=88"]).expect("flags");
        assert!(sizes_on(&cli, topology()).is_err());
    }

    /// Catches a `--lease-swap-*` flag that does not reach the driver's swap watch (in
    /// its unit: MiB, percent, milliseconds), a default that differs from the driver's
    /// documented `SwapKill::DEFAULT`, and a poll of 0 accepted (a busy loop).
    #[cfg(target_os = "linux")]
    #[test]
    fn swap_kill_flags_reach_the_configuration() {
        use kbf_driver_container::SwapKill;
        let cli = parse(&[
            "--driver=container",
            "--lease-swap-kill-mib=3",
            "--lease-swap-kill-percent=40",
            "--lease-swap-poll-ms=250",
        ])
        .expect("flags");
        assert_eq!(
            container::swap_kill(&cli),
            SwapKill {
                floor_bytes: 3 << 20,
                percent: 40,
                every: Duration::from_millis(250),
            }
        );
        let defaults = parse(&["--driver=container"]).expect("flags");
        assert_eq!(container::swap_kill(&defaults), SwapKill::DEFAULT);
        assert!(parse(&["--driver=container", "--lease-swap-poll-ms=0"]).is_err());
    }

    // This test binary answers as kbf-testkit's `kbf-act`, or as its fake `xcrun`,
    // `xcodebuild` or `sandbox-exec`, when run through a link of that name: the
    // programs the tests here (and src/startup_tests.rs) give the daemon to run.
    kbf_testkit::act_test_hook!();

    /// Makes `path` a link to this test binary, which answers as the fake its base name
    /// names, with `rules` beside it (see `kbf_testkit::fake`).
    pub(crate) fn fake(path: &Path, rules: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        kbf_testkit::act::link_self(path).expect("link");
        std::fs::write(format!("{}.rules", path.display()), rules).expect("rules");
    }

    /// A fresh directory for one test, under the test binary's directory.
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-node-unit")
            .join(format!("{name}-{}", std::process::id()));
        // Absent unless a run with this pid left it.
        let _ = kbf_outputs::remove_tree(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// A CA and a client certificate signed by it, written to `dir` as the flags name.
    pub(crate) fn tls_files(dir: &Path) {
        use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
        let key = KeyPair::generate().expect("key");
        let cert = CertificateParams::new(Vec::new())
            .expect("params")
            .signed_by(&key, &ca)
            .expect("sign");
        std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
        std::fs::write(dir.join("node.pem"), cert.pem()).expect("write");
        std::fs::write(dir.join("node.key"), key.serialize_pem()).expect("write");
    }

    /// Catches: the native driver's Xcode survey not handed to the daemon (issue #164),
    /// so the node's `NodeStatus` lists no Xcode at all, ready or not, and its Hello
    /// advertises none: the silent removal an Xcode that is not ready must never get.
    /// The Xcode here is an empty app, which no `xcodebuild` accepts (on Linux there is
    /// none), so the survey lists it as not ready, with why.
    #[tokio::test]
    async fn the_native_daemon_reports_every_installed_xcode() {
        let dir = scratch("native");
        tls_files(&dir);
        let apps = dir.join("Applications");
        let app = apps.join("Xcode_1.app");
        std::fs::create_dir_all(app.join("Contents/Developer")).expect("app");
        let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
        let cli = Cli::try_parse_from([
            "kbf-daemon".to_owned(),
            "--server=https://127.0.0.1:1".to_owned(),
            flag("ca-cert", &dir.join("ca.pem")),
            flag("cert", &dir.join("node.pem")),
            flag("key", &dir.join("node.key")),
            "--node-id=mac-1".to_owned(),
            "--driver=native".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            flag("data-dir", &dir),
            flag("xcode-apps", &apps),
        ])
        .expect("flags");
        let mut config = native_config(&cli).expect("config");
        // Not the runner's own (on macOS): the start sweeps them.
        config.user_folders = None;
        let (daemon, mut reports) =
            native_with(&cli, config, &dir.join("no-xcrun")).expect("the native daemon");
        // Not surveyed yet, or already surveyed (this one fails at once): either way
        // listed. The gated tests (xcode_watch, startup_tests) hold the survey.
        let listed = daemon.node_status().xcodes;
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].app, app.display().to_string());
        let xcodes = surveyed(&mut reports).await.xcodes;
        assert_eq!(xcodes.len(), 1, "{xcodes:?}");
        assert_eq!(xcodes[0].app, app.display().to_string());
        assert_eq!(xcodes[0].state(), XcodeState::Failed);
        assert!(!xcodes[0].reason.is_empty(), "{xcodes:?}");
        drop((daemon, reports));
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// Catches (CEO decision on issue #164): the native daemon removing `xcrun`'s cache
    /// before its first survey. Every `xcrun` lookup of the survey then has nothing
    /// cached and takes seconds, while no Xcode is ready: on the macOS runner (15
    /// `Xcode*.app`) that survey took over 30 s, against about 7 s with the cache kept. The survey runs under the sandbox, so
    /// what a lease wrote there reaches only sandboxed tools. The Xcode's own
    /// `xcodebuild` is a fake that answers build `CACHE` while the cache is there, and
    /// fails once it is gone.
    #[tokio::test]
    async fn the_first_survey_finds_the_xcrun_cache_kept() {
        let dir = scratch("forget");
        tls_files(&dir);
        let (temp, cache) = (dir.join("T"), dir.join("C"));
        std::fs::create_dir_all(&temp).expect("T");
        std::fs::create_dir_all(&cache).expect("C");
        std::fs::write(temp.join("xcrun_db"), b"a lease's").expect("cache");
        let apps = dir.join("Applications");
        let program = apps
            .join("Xcode_1.app/Contents/Developer")
            .join(xcode::XCODEBUILD);
        fake(
            &program,
            &format!(
                "*\t-\twait-for {}/xcrun_db 0 ; echo Build version CACHE\n",
                temp.display()
            ),
        );
        // On macOS the survey runs under the real sandbox, which lets it write only in
        // its lease directory and under /dev: the fake's log of its runs (beside it,
        // see kbf_testkit::fake) goes to /dev/null.
        std::os::unix::fs::symlink("/dev/null", format!("{}.log", program.display()))
            .expect("log link");
        let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
        let cli = Cli::try_parse_from([
            "kbf-daemon".to_owned(),
            "--server=https://127.0.0.1:1".to_owned(),
            flag("ca-cert", &dir.join("ca.pem")),
            flag("cert", &dir.join("node.pem")),
            flag("key", &dir.join("node.key")),
            "--node-id=mac-1".to_owned(),
            "--driver=native".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            flag("data-dir", &dir),
            flag("xcode-apps", &apps),
        ])
        .expect("flags");
        let mut config = native_config(&cli).expect("config");
        config.user_folders =
            kbf_driver_native::user_folders::UserFolders::new(temp.clone(), cache);
        let (daemon, mut reports) =
            native_with(&cli, config, &dir.join("no-xcrun")).expect("the native daemon");
        let xcodes = surveyed(&mut reports).await.xcodes;
        assert_eq!(xcodes.len(), 1, "{xcodes:?}");
        assert_eq!(xcodes[0].build, "CACHE", "{xcodes:?}");
        assert!(
            temp.join("xcrun_db").exists(),
            "the start removed the cache"
        );
        drop((daemon, reports));
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// Catches (CEO decision on issue #164): the native daemon's survey of its Xcodes
    /// asking a question outside the actions' sandbox, `xcrun`'s lookup above all (it
    /// reads and fills a cache leases can write), or with an `xcrun` other than the one
    /// the daemon warms. The sandbox program is a fake that marks what it runs (its
    /// `-D KBF_LEASE=<dir>` in the command's environment); the Xcode's `xcodebuild` and
    /// the `xcrun` are fakes that log each run and the mark, empty when unmarked. The
    /// warm-up of the Xcode the survey finds ready runs too, also
    /// sandboxed; the test waits for it to end. Also catches the warm-up not run, or
    /// run before the survey's questions are answered (its lookups would make the
    /// survey's miss): the fake `xcodebuild` answers after half a second, so a warm-up
    /// started with the daemon logs before it.
    #[tokio::test]
    async fn the_daemons_survey_runs_under_the_sandbox() {
        let dir = scratch("sandboxed");
        tls_files(&dir);
        let log = dir.join("ran");
        let sandbox_exec = dir.join("sandbox-exec");
        fake(&sandbox_exec, "");
        // `<tool> <the sandbox's lease directory, empty when unsandboxed> <arguments>`.
        let logged = |tool: &str| format!("append {} {tool} ${{KBF_LEASE}} $*", log.display());
        let apps = dir.join("Applications");
        let xcodebuild = apps
            .join("Xcode_1.app/Contents/Developer")
            .join(xcode::XCODEBUILD);
        fake(
            &xcodebuild,
            &format!(
                "*\t-\tsleep 0.5 ; {} ; echo Build version 1A1\n",
                logged("xcodebuild")
            ),
        );
        let xcrun = dir.join("bin/xcrun");
        fake(
            &xcrun,
            &format!("*\t-\t{} ; echo /x/clang\n", logged("xcrun")),
        );
        let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
        let cli = Cli::try_parse_from([
            "kbf-daemon".to_owned(),
            "--server=https://127.0.0.1:1".to_owned(),
            flag("ca-cert", &dir.join("ca.pem")),
            flag("cert", &dir.join("node.pem")),
            flag("key", &dir.join("node.key")),
            "--node-id=mac-1".to_owned(),
            "--driver=native".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            flag("data-dir", &dir),
            flag("xcode-apps", &apps),
        ])
        .expect("flags");
        let mut config = native_config(&cli).expect("config");
        // Not the runner's own (on macOS): the start sweeps them.
        config.user_folders = None;
        config.isolation = kbf_driver_native::network::Isolation::Sandbox(sandbox_exec);
        let (daemon, mut reports) = native_with(&cli, config, &xcrun).expect("the native daemon");
        let xcodes = surveyed(&mut reports).await.xcodes;
        assert_eq!(xcodes.len(), 1, "{xcodes:?}");
        assert_eq!(xcodes[0].state(), XcodeState::Ready, "{xcodes:?}");
        let leases = std::fs::canonicalize(dir.join("leases")).expect("real");
        let survey = leases.join(kbf_driver_native::SURVEY_DIR);
        assert!(!survey.exists(), "the survey's directory stays");
        // The warm-up removes its directory when it ends: waited for up to 20 s
        // (without a branch of this test's own, which coverage would count).
        let warm = leases.join("lease-warm-up");
        (0..1000)
            .take_while(|_| warm.exists())
            .for_each(|_| std::thread::sleep(Duration::from_millis(20)));
        let ran = std::fs::read_to_string(&log).expect("ran");
        let at = |lease: &Path, what: &str| format!("{} {what}", lease.display());
        let warmed = format!("xcrun {}", at(&warm, ""));
        let asked = at(&survey, "");
        assert!(
            ran.lines().any(|l| l.starts_with(&warmed)),
            "no warm-up: {ran}"
        );
        let asked_first = ran
            .lines()
            .take_while(|l| !l.starts_with(&warmed))
            .filter(|l| l.contains(&asked))
            .count();
        assert_eq!(
            asked_first, 4,
            "the warm-up ran before the survey's answers: {ran}"
        );
        // The survey's questions, asked at once, come before the warm-up's lookups.
        let mut surveyed: Vec<String> = ran
            .lines()
            .filter(|l| l.starts_with("xcodebuild ") || l.ends_with(" --find clang"))
            .take(4)
            .map(str::to_owned)
            .collect();
        surveyed.sort();
        assert_eq!(
            surveyed,
            [
                format!("xcodebuild {}", at(&survey, "-checkFirstLaunchStatus")),
                format!("xcodebuild {}", at(&survey, "-license check")),
                format!("xcodebuild {}", at(&survey, "-version")),
                format!("xcrun {}", at(&survey, "--find clang")),
            ],
            "{ran}"
        );
        assert!(
            ran.lines()
                .all(|l| l.starts_with("xcodebuild ") || l.starts_with("xcrun ")),
            "{ran}"
        );
        let unsandboxed: Vec<&str> = ran
            .lines()
            .filter(|l| !l.contains(&*leases.to_string_lossy()))
            .collect();
        assert_eq!(unsandboxed, Vec::<&str>::new(), "{ran}");
        drop((daemon, reports));
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// Catches: `--driver` taken once only (a node could not run two runtimes), not
    /// required, a driver given twice set up twice, and the node's own driver set up
    /// after another (native always comes first).
    #[test]
    fn drivers_come_once_each_with_the_nodes_own_first() {
        let cli = parse(&["--driver=fake", "--driver=native", "--driver=fake"]).expect("flags");
        assert_eq!(drivers(&cli), [Driver::Native, Driver::Fake]);
        let cli = parse(&["--driver=fake", "--driver=container"]).expect("flags");
        assert_eq!(drivers(&cli), [Driver::Container, Driver::Fake]);
        assert!(parse(&[]).is_err(), "no --driver");
    }

    /// Catches: a daemon that starts with two drivers serving one lease kind (each
    /// Start of that kind would run on whichever comes first, silently), and one whose
    /// refusal does not name both drivers and the kind. Native and fake both serve
    /// `action`.
    #[tokio::test]
    async fn two_drivers_serving_one_kind_refuse_to_start() {
        let dir = scratch("two-drivers");
        tls_files(&dir);
        let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
        let cli = |drivers: &[&str]| {
            let flags = [
                "kbf-daemon".to_owned(),
                "--server=https://127.0.0.1:1".to_owned(),
                flag("ca-cert", &dir.join("ca.pem")),
                flag("cert", &dir.join("node.pem")),
                flag("key", &dir.join("node.key")),
                "--node-id=mac-1".to_owned(),
                "--cas=https://127.0.0.1:1".to_owned(),
                flag("data-dir", &dir),
                flag("xcode-apps", &dir.join("Applications")),
            ];
            let drivers = drivers.iter().map(|d| format!("--driver={d}"));
            Cli::try_parse_from(flags.into_iter().chain(drivers)).expect("flags")
        };
        let both = cli(&["fake", "native"]);
        let native = || {
            let mut config = native_config(&both)?;
            // Not the runner's own (on macOS): the start sweeps them.
            config.user_folders = None;
            Ok(config)
        };
        let why = node(&both, native, &dir.join("no-xcrun"))
            .err()
            .expect("two drivers serve action")
            .to_string();
        assert_eq!(
            why,
            "drivers \"native\" and \"fake\" both serve lease kind action: each kind must \
             have one driver on a node"
        );
        let fakes = cli(&["fake", "fake"]);
        let (daemon, reports) = node(&fakes, native, Path::new("none")).expect("one fake runtime");
        assert!(reports.is_none());
        drop(daemon);
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// Catches: a plain-text `--cas` accepted (the daemon would send blobs where no
    /// certificate admits them, or fall back to an unauthenticated port), a CAS URL of
    /// another scheme accepted, `--cas` not required, and an https URL that does not
    /// read the daemon's own TLS files.
    #[tokio::test]
    async fn the_cas_url_must_be_https() {
        for url in ["http://front:8981", "ftp://front", "front:8981"] {
            let cli = parse(&["--driver=native", &format!("--cas={url}")]).expect("flags");
            let why = transfer::engine(&cli).expect_err(url).to_string();
            assert!(why.contains("must be an https:// URL"), "{url}: {why}");
        }
        let none = parse(&["--driver=native"]).expect("flags");
        let why = transfer::engine(&none).expect_err("no --cas").to_string();
        assert!(why.contains("--cas is required"), "{why}");
        // https reads the daemon's TLS files, which these flags name but do not hold.
        let https = parse(&["--driver=native", "--cas=https://front:8981"]).expect("flags");
        let why = transfer::engine(&https)
            .expect_err("no TLS files")
            .to_string();
        assert!(why.contains("ca.pem"), "{why}");
    }

    /// The three liveness flags, each different from the others and from the defaults, so
    /// that a default, or two values swapped, differs from what was asked.
    const LIVENESS_FLAGS: [&str; 3] = [
        "--storage-dial-timeout-ms=7000",
        "--storage-ping-interval-ms=3000",
        "--storage-ping-timeout-ms=2000",
    ];

    fn asked() -> Liveness {
        Liveness {
            connect: Duration::from_millis(7000),
            keepalive_interval: Duration::from_millis(3000),
            keepalive_timeout: Duration::from_millis(2000),
        }
    }

    /// Catches the storage-liveness flags stopping short of the nodes the daemon dials
    /// storage through: `start` handing its plane the defaults instead of the checked
    /// flags, or interval and timeout swapped on the way. Both the plane factory's
    /// argument (`start_with`'s and `start`'s path through `start_over`) and the
    /// `TlsNodes` that `farm` builds are read back.
    #[test]
    fn the_storage_liveness_flags_reach_the_plane_and_its_nodes() {
        assert_ne!(asked(), Liveness::default());
        let dir = scratch("liveness");
        tls_files(&dir);
        let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
        let mut flags = vec![
            "kbf-daemon".to_owned(),
            "--server=https://127.0.0.1:1".to_owned(),
            flag("ca-cert", &dir.join("ca.pem")),
            flag("cert", &dir.join("node.pem")),
            flag("key", &dir.join("node.key")),
            "--node-id=mac-1".to_owned(),
            // A driver that moves blobs, so that the plane is made.
            "--driver=native".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            flag("data-dir", &dir),
            flag("xcode-apps", &dir.join("Applications")),
        ];
        flags.extend(LIVENESS_FLAGS.map(str::to_owned));
        let cli = Cli::try_parse_from(flags).expect("flags");
        // `farm` builds the nodes from what it is given; what it holds is read back.
        // The lazy channel to --cas needs a runtime's context.
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        {
            let _context = runtime.enter();
            let (_, held) = farm(&cli, &Arc::new(MapHolder::new()), asked()).expect("the plane");
            assert_eq!(held, asked());
        }
        drop(runtime);
        // start_over checks the flags and hands the plane factory the checked values.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let record = Arc::clone(&seen);
        let handle = start_over(&cli, None, move |cli, liveness| {
            *record.lock().expect("lock") = Some(liveness);
            remote(cli)
        })
        .expect("start");
        drop(handle);
        assert_eq!(*seen.lock().expect("lock"), Some(asked()));
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// The plane factory the update-dir test hands `start_over` three times: one function
    /// item, so the three starts are one instantiation (the coverage ratchet counts the
    /// best instantiation, which must run every arm of the match on the map and the flag).
    fn plane(cli: &Cli, _: Liveness) -> Result<Arc<kbf_transfer::Transfers>, Error> {
        remote(cli)
    }

    /// Catches a flag taken and ignored, and an update client left off: with a storage map
    /// and `--update-dir` the daemon gets its update client; without a map (the in-process
    /// daemon of the all-in-one binary) `--update-dir` is refused instead of silently doing
    /// nothing; with neither, nothing is asked.
    ///
    /// Mutants seen red: the refusing arm dropped (the second start succeeds); the link
    /// made whatever the flag says (the third start fails on the missing directory).
    #[test]
    fn the_update_dir_is_for_a_daemon_with_a_map_and_refused_without_one() {
        let dir = scratch("update-dir");
        tls_files(&dir);
        let flag = |name: &str, path: &Path| format!("--{name}={}", path.display());
        let base = vec![
            "kbf-daemon".to_owned(),
            "--server=https://127.0.0.1:1".to_owned(),
            flag("ca-cert", &dir.join("ca.pem")),
            flag("cert", &dir.join("node.pem")),
            flag("key", &dir.join("node.key")),
            "--node-id=mac-1".to_owned(),
            "--driver=native".to_owned(),
            "--cas=https://127.0.0.1:1".to_owned(),
            flag("data-dir", &dir),
            flag("xcode-apps", &dir.join("Applications")),
        ];
        let mut asked = base.clone();
        asked.push(flag("update-dir", &dir.join("update")));
        let with_dir = Cli::try_parse_from(asked).expect("flags");
        let base_flags = base.clone();
        let without = Cli::try_parse_from(base).expect("flags");
        let started = start_over(&with_dir, Some(Arc::new(MapHolder::new())), plane);
        drop(started.expect("a daemon with a map takes an update directory"));
        let why = start_over(&with_dir, None, plane).err().expect("refused");
        assert!(
            why.to_string().contains("--update-dir is not taken"),
            "{why}"
        );
        drop(start_over(&without, None, plane).expect("no flag, no client"));
        // The fourth start fails inside `node_over`: this instantiation also takes the
        // error return of that call, so it is the one the ratchet counts as fully run.
        let mut broken = base_flags.clone();
        broken.push("--lease-placement=numa-node".to_owned());
        let broken = Cli::try_parse_from(broken).expect("flags");
        let why = start_over(&broken, None, plane).err().expect("refused");
        assert!(why.to_string().contains("only --driver container"), "{why}");
        kbf_outputs::remove_tree(&dir).expect("clean");
    }

    /// Catches a daemon that starts with storage-liveness flags it cannot honour: a zero
    /// value, or a ping timeout past the interval, is refused by `start` (and by
    /// `start_with`, as the same check) before any driver is set up, naming the flag.
    #[test]
    fn start_refuses_unusable_storage_liveness_flags() {
        for (flag, said) in [
            (
                "--storage-dial-timeout-ms=0",
                "--storage-dial-timeout-ms must be at least 1",
            ),
            (
                "--storage-ping-interval-ms=0",
                "--storage-ping-interval-ms must be at least 1",
            ),
            (
                "--storage-ping-timeout-ms=0",
                "--storage-ping-timeout-ms must be at least 1",
            ),
            (
                "--storage-ping-timeout-ms=10001",
                "is longer than --storage-ping-interval-ms",
            ),
            (
                "--storage-dial-timeout-ms=3600001",
                "--storage-dial-timeout-ms must be at most 3600000",
            ),
            (
                "--storage-ping-interval-ms=3600001",
                "--storage-ping-interval-ms must be at most 3600000",
            ),
            (
                "--storage-ping-timeout-ms=3600001",
                "--storage-ping-timeout-ms must be at most 3600000",
            ),
        ] {
            let cli = parse(&["--driver=fake", flag]).expect("flags parse");
            for refused in [start(&cli).err(), start_with(&cli, remote).err()] {
                let why = refused.expect(flag).to_string();
                assert!(why.contains(said), "{flag}: {why}");
            }
        }
    }
}
