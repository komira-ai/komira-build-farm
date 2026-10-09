//! `kbf-daemon`: the worker daemon. See the `kbf-daemon` library for what it does.
//!
//! Configuration is by flags only (`kbf-daemon --help`). `--driver` picks how leases
//! run: `fake` (nothing runs; for bring-up), `container` (rootless Podman; Linux) or
//! `native` (plain processes; for Macs). The two real drivers read and write blobs
//! through the front's REAPI listener named by `--cas`, and make lease directories
//! under `--scratch`. A Mac in komira's pool runs, for example:
//!
//! ```text
//! kbf-daemon --driver native --server https://front:7070 --cas http://front:8980 \
//!   --ca-cert ca.pem --cert node.pem --key node.key --node-id mac-studio-1 \
//!   --scratch /var/kbf/leases --label pool=darwin-sized
//! ```
//!
//! Exits non-zero when the node cannot be detected or the configuration cannot be
//! loaded; otherwise runs until SIGTERM or SIGINT, then exits zero. Leases still
//! running then are abandoned (their processes killed, their directories removed);
//! the scheduler places them again.
//!
//! The log goes to stderr, in color only when stderr is a terminal: under launchd or
//! systemd it is a file or a journal, where escape codes are noise (issue #170). The
//! daemon does not rotate it; the service manager that owns stderr does. systemd's
//! journal rotates by its own size limits; launchd's `StandardErrorPath` file is
//! emptied by `kbf-mac-provision` at each restart and upgrade (see
//! `docs/design/mac-node-provisioning.md`, section 5.7).

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kbf_daemon::{
    Args, CasClient, DAEMON_VERSION, Daemon, DaemonConfig, DriverReport, FakeRuntime, NodeReport,
    Runtime,
};
use kbf_driver_native::{MemoryPolicy, NativeConfig, NativeRuntime, xcode, xcode_watch};
use kbf_outputs::OutputLimits;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::watch;
use tonic::transport::Endpoint;

/// The `kbf-daemon` command line.
#[derive(Clone, Debug, clap::Parser)]
#[command(name = "kbf-daemon", version = DAEMON_VERSION, about = "The kbf worker daemon.")]
struct Cli {
    #[command(flatten)]
    daemon: Args,
    /// How leases run.
    #[arg(long, value_enum)]
    driver: Driver,
    /// The front's REAPI listener, `http://` or `https://` (which presents this
    /// daemon's certificate), that the container and native drivers read and write
    /// blobs through.
    #[arg(long)]
    cas: Option<String>,
    /// The directory lease directories are made in (container and native drivers).
    #[arg(long)]
    scratch: Option<PathBuf>,
    #[command(flatten)]
    outputs: OutputLimits,
    /// The daemon's delegated cgroup for actions, from the cgroup root (container).
    #[arg(long)]
    cgroup_parent: Option<String>,
    /// A lease's processes are killed past this percentage of its booked memory...
    #[arg(long, default_value_t = MemoryPolicy::DEFAULT.percent)]
    memory_limit_percent: u64,
    /// ...plus this many MiB (native). A lease that booked nothing has no limit.
    #[arg(long, default_value_t = MemoryPolicy::DEFAULT.headroom_bytes >> 20)]
    memory_headroom_mib: u64,
    /// How often a lease's memory is measured, in milliseconds (native).
    #[arg(long, default_value_t = 250)]
    memory_poll_ms: u64,
    /// The directory searched for `Xcode*.app` (native). Each Xcode that answers
    /// `xcodebuild -version`, `-license check` and `-checkFirstLaunchStatus` and `xcrun
    /// --find clang` (each within a minute) is ready: it is reported as an `xcode`
    /// entry, and an action that names its build runs with it as `DEVELOPER_DIR`. One
    /// that is not is listed in the node's status with why and the command that fixes
    /// it, and logged at WARN.
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
    /// A directory holding `passwd`, `subuid` and `subgid` that the container
    /// driver's startup check reads instead of `/etc`'s. For tests of that check only.
    #[arg(long, hide = true)]
    id_files: Option<PathBuf>,
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
}

type Error = Box<dyn std::error::Error>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    match start(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-daemon: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Builds the driver `--driver` names and serves until a signal; returns an error that
/// stopped the daemon from starting.
fn start(cli: &Cli) -> Result<(), Error> {
    let tokio = tokio::runtime::Runtime::new()?;
    // The CAS channel is made lazily, which needs the runtime's context.
    let _context = tokio.enter();
    match cli.driver {
        Driver::Fake => serve(
            cli,
            &tokio,
            Arc::new(FakeRuntime::new(Duration::ZERO)),
            None,
        ),
        Driver::Native => {
            let runtime = NativeRuntime::new(native_config(cli)?, Arc::new(cas_client(cli)?))?;
            let runtime = Arc::new(runtime);
            let watched = Arc::clone(&runtime);
            let (probe, every) = xcode_watch_args(cli);
            let (driver, _) = xcode_watch::watch(
                cli.xcode_apps.clone(),
                probe,
                every,
                Box::new(move |xcodes| watched.apply_xcodes(xcodes)),
            );
            serve(cli, &tokio, runtime, Some(driver))
        }
        Driver::Container => container::start(cli, &tokio),
    }
}

/// Runs the daemon with `runtime` until SIGTERM or SIGINT. The node report is the
/// detected entries and the labels, plus the newest entries `driver` sends, if any.
fn serve<R: Runtime>(
    cli: &Cli,
    tokio: &tokio::runtime::Runtime,
    runtime: Arc<R>,
    driver: Option<watch::Receiver<DriverReport>>,
) -> Result<(), Error> {
    let report = NodeReport::detect(&[runtime.driver()])?.with_entries(cli.daemon.label_entries());
    let mut daemon = Daemon::new(DaemonConfig::from_args(&cli.daemon), runtime, report)?;
    if let Some(driver) = driver {
        daemon = daemon.with_driver_report(driver);
    }
    let term = signal(SignalKind::terminate())?;
    let int = signal(SignalKind::interrupt())?;
    tokio.block_on(daemon.run(shutdown(term, int)));
    Ok(())
}

/// Completes on SIGTERM or SIGINT.
async fn shutdown(mut term: Signal, mut int: Signal) {
    let name = tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
    };
    tracing::info!("{name}: shutting down");
}

/// The lease directory root, which the real drivers require.
fn scratch(cli: &Cli) -> Result<PathBuf, Error> {
    let scratch = cli
        .scratch
        .clone()
        .ok_or("--scratch is required by this driver")?;
    if !scratch.is_absolute() {
        return Err(format!("--scratch {} must be absolute", scratch.display()).into());
    }
    Ok(scratch)
}

fn native_config(cli: &Cli) -> Result<NativeConfig, Error> {
    let mut config = NativeConfig::new(scratch(cli)?);
    config.outputs = cli.outputs;
    config.memory = MemoryPolicy {
        percent: cli.memory_limit_percent,
        headroom_bytes: cli.memory_headroom_mib.saturating_mul(1 << 20),
    };
    config.poll = Duration::from_millis(cli.memory_poll_ms.max(1));
    Ok(config)
}

/// How the native driver asks its Xcodes (the system's tools, and the Metal toolchain
/// if required), and how often it asks again (at least every second).
fn xcode_watch_args(cli: &Cli) -> (xcode::Probe, Duration) {
    let probe = xcode::Probe::system(cli.require_metal_toolchain);
    (probe, Duration::from_secs(cli.xcode_recheck_secs.max(1)))
}

/// A client of the CAS `--cas` names. Connects on first use.
fn cas_client(cli: &Cli) -> Result<CasClient, Error> {
    let url = cli
        .cas
        .clone()
        .ok_or("--cas is required by the container and native drivers")?;
    let mut endpoint = Endpoint::from_shared(url.clone())?;
    if url.starts_with("https://") {
        endpoint = endpoint.tls_config(DaemonConfig::from_args(&cli.daemon).tls.load()?)?;
    } else if !url.starts_with("http://") {
        return Err(format!("--cas {url:?} must be an http:// or https:// URL").into());
    }
    Ok(CasClient::new(endpoint.connect_lazy()))
}

#[cfg(target_os = "linux")]
mod container {
    use std::sync::Arc;

    use kbf_driver_container::{IdFiles, OutputLimits, PodmanConfig, PodmanRuntime};

    use super::{Cli, Error, cas_client, scratch, serve};

    /// Builds the container driver, over the daemon's CAS client, and serves with it.
    pub(super) fn start(cli: &Cli, tokio: &tokio::runtime::Runtime) -> Result<(), Error> {
        let parent = cli
            .cgroup_parent
            .clone()
            .ok_or("--cgroup-parent is required by the container driver")?;
        // Every container's ids are this user's subordinate ids (`--userns=nomap`).
        let files = cli
            .id_files
            .as_deref()
            .map_or_else(IdFiles::system, IdFiles::in_dir);
        kbf_driver_container::check_daemon_user(&files)?;
        let mut config = PodmanConfig::new(scratch(cli)?, parent);
        config.outputs = OutputLimits {
            max_depth: cli.outputs.max_depth,
            max_entries: cli.outputs.max_entries,
            max_bytes: cli.outputs.max_bytes,
            max_stdio_bytes: cli.outputs.max_stdio_bytes,
        };
        let runtime = PodmanRuntime::new(config, Arc::new(cas_client(cli)?))?;
        serve(cli, tokio, Arc::new(runtime), None)
    }
}

#[cfg(not(target_os = "linux"))]
mod container {
    use super::{Cli, Error};

    pub(super) fn start(_cli: &Cli, _tokio: &tokio::runtime::Runtime) -> Result<(), Error> {
        Err("the container driver runs on Linux only; use --driver native".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "--scratch=/var/kbf/leases",
            "--memory-limit-percent=200",
            "--memory-headroom-mib=64",
            "--memory-poll-ms=0",
            "--output-max-bytes=99",
            "--xcode-apps=/var/kbf/apps",
        ])
        .expect("flags");
        assert_eq!(cli.driver, Driver::Native);
        let config = native_config(&cli).expect("config");
        assert_eq!(config.scratch, PathBuf::from("/var/kbf/leases"));
        assert_eq!(config.memory.limit(1 << 20), Some((2 << 20) + (64 << 20)));
        assert_eq!(config.poll, Duration::from_millis(1));
        assert_eq!(config.outputs.max_bytes, 99);
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
        let defaults = parse(&["--driver=native", "--scratch=/s"]).expect("flags");
        assert_eq!(
            native_config(&defaults).expect("config").memory,
            MemoryPolicy::DEFAULT
        );
        assert_eq!(defaults.xcode_apps, PathBuf::from("/Applications"));
        let relative = parse(&["--driver=native", "--scratch=leases"]).expect("flags");
        assert!(native_config(&relative).is_err());
        let missing = parse(&["--driver=native"]).expect("flags");
        assert!(native_config(&missing).is_err());
    }

    /// Catches: a CAS URL of another scheme accepted, and `--cas` not required.
    #[tokio::test]
    async fn the_cas_url_must_be_http_or_https() {
        let http = parse(&["--driver=native", "--cas=http://front:8980"]).expect("flags");
        assert!(cas_client(&http).is_ok());
        let other = parse(&["--driver=native", "--cas=ftp://front"]).expect("flags");
        let why = cas_client(&other).expect_err("ftp").to_string();
        assert!(why.contains("must be an http"), "{why}");
        let none = parse(&["--driver=native"]).expect("flags");
        assert!(cas_client(&none).is_err());
        // https reads the daemon's TLS files, which these flags name but do not hold.
        let https = parse(&["--driver=native", "--cas=https://front:8980"]).expect("flags");
        assert!(cas_client(&https).is_err());
    }
}
