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

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kbf_daemon::{Args, CasClient, Daemon, DaemonConfig, FakeRuntime, NodeReport, Runtime};
use kbf_driver_native::{MemoryPolicy, NativeConfig, NativeRuntime};
use kbf_outputs::OutputLimits;
use tonic::transport::Endpoint;

/// The `kbf-daemon` command line.
#[derive(Clone, Debug, clap::Parser)]
#[command(name = "kbf-daemon", version, about = "The kbf worker daemon.")]
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
        Driver::Fake => serve(cli, &tokio, Arc::new(FakeRuntime::new(Duration::ZERO)), []),
        Driver::Native => {
            let runtime = NativeRuntime::new(native_config(cli)?, Arc::new(cas_client(cli)?))?;
            let capabilities = runtime.capabilities();
            serve(cli, &tokio, Arc::new(runtime), capabilities)
        }
        Driver::Container => container::start(cli, &tokio),
    }
}

/// Runs the daemon with `runtime` until SIGTERM or SIGINT. `extra` joins the node
/// report, after the detected entries and the labels.
fn serve<R: Runtime>(
    cli: &Cli,
    tokio: &tokio::runtime::Runtime,
    runtime: Arc<R>,
    extra: impl IntoIterator<Item = (String, String)>,
) -> Result<(), Error> {
    let report = NodeReport::detect(&[runtime.driver()])?
        .with_entries(cli.daemon.label_entries())
        .with_entries(extra);
    let daemon = Daemon::new(DaemonConfig::from_args(&cli.daemon), runtime, report)?;
    tokio.block_on(daemon.run(shutdown()));
    Ok(())
}

/// Completes on SIGTERM or SIGINT.
async fn shutdown() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::error!("cannot listen for SIGTERM and SIGINT; running until killed");
        return std::future::pending().await;
    };
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

    use kbf_daemon::{Cas as _, CasClient};
    use kbf_driver_container::{Cas, CasError, OutputLimits, PodmanConfig, PodmanRuntime};
    use kbf_proto::reapi::Digest;

    use super::{Cli, Error, cas_client, scratch, serve};

    /// Builds the container driver and serves with it.
    pub(super) fn start(cli: &Cli, tokio: &tokio::runtime::Runtime) -> Result<(), Error> {
        let parent = cli
            .cgroup_parent
            .clone()
            .ok_or("--cgroup-parent is required by the container driver")?;
        let mut config = PodmanConfig::new(scratch(cli)?, parent);
        config.outputs = OutputLimits {
            max_depth: cli.outputs.max_depth,
            max_entries: cli.outputs.max_entries,
            max_bytes: cli.outputs.max_bytes,
        };
        let runtime = PodmanRuntime::new(config, Arc::new(ContainerCas(cas_client(cli)?)))?;
        serve(cli, tokio, Arc::new(runtime), [])
    }

    /// The daemon's CAS client behind the container driver's own `Cas` trait, until
    /// that driver moves onto the daemon's (a follow-up).
    pub(super) struct ContainerCas(pub(super) CasClient);

    impl Cas for ContainerCas {
        async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
            self.0.get(digest).await.map_err(convert)
        }

        async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
            self.0.put(bytes).await.map_err(convert)
        }
    }

    /// The daemon's CAS error as the container driver's. That driver has no
    /// "unreachable": it reports every CAS error as a failed lease, so an outage is
    /// named in a missing-blob message.
    pub(super) fn convert(error: kbf_daemon::CasError) -> CasError {
        match error {
            kbf_daemon::CasError::Missing(blob) => CasError::Missing(blob),
            kbf_daemon::CasError::Corrupt(blob, actual) => CasError::Corrupt(blob, actual),
            kbf_daemon::CasError::Unavailable(blob, why) => {
                CasError::Missing(format!("{blob} (the CAS call failed: {why})"))
            }
        }
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
    /// memory limit, poll and output limits), and a relative or missing scratch
    /// directory accepted.
    #[test]
    fn native_flags_reach_the_configuration() {
        let cli = parse(&[
            "--driver=native",
            "--scratch=/var/kbf/leases",
            "--memory-limit-percent=200",
            "--memory-headroom-mib=64",
            "--memory-poll-ms=0",
            "--output-max-bytes=99",
        ])
        .expect("flags");
        assert_eq!(cli.driver, Driver::Native);
        let config = native_config(&cli).expect("config");
        assert_eq!(config.scratch, PathBuf::from("/var/kbf/leases"));
        assert_eq!(config.memory.limit(1 << 20), Some((2 << 20) + (64 << 20)));
        assert_eq!(config.poll, Duration::from_millis(1));
        assert_eq!(config.outputs.max_bytes, 99);
        let defaults = parse(&["--driver=native", "--scratch=/s"]).expect("flags");
        assert_eq!(
            native_config(&defaults).expect("config").memory,
            MemoryPolicy::DEFAULT
        );
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

    /// Catches: a CAS outage reported to the container driver as a corrupt blob, or
    /// its reason lost; and a missing or corrupt blob changing kind on the way.
    #[cfg(target_os = "linux")]
    #[test]
    fn container_cas_errors_keep_their_meaning() {
        use kbf_driver_container::CasError;
        assert_eq!(
            container::convert(kbf_daemon::CasError::Missing("a/1".into())),
            CasError::Missing("a/1".into())
        );
        assert_eq!(
            container::convert(kbf_daemon::CasError::Corrupt("a/1".into(), "b/1".into())),
            CasError::Corrupt("a/1".into(), "b/1".into())
        );
        let outage = container::convert(kbf_daemon::CasError::Unavailable(
            "a/1".into(),
            "down".into(),
        ));
        assert_eq!(
            outage,
            CasError::Missing("a/1 (the CAS call failed: down)".into())
        );
    }

    /// Catches: the adapter not reaching the daemon's client, or an outage on either
    /// call surfacing as anything but the converted error.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_container_cas_calls_the_daemons_client() {
        use kbf_driver_container::{Cas as _, CasError};
        // A port nothing listens on.
        let channel = Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
        let cas = container::ContainerCas(CasClient::new(channel));
        let digest = kbf_daemon::cas::digest_of(b"x");
        let got = cas.get(&digest).await;
        assert!(
            matches!(&got, Err(CasError::Missing(m)) if m.contains("CAS call failed")),
            "{got:?}"
        );
        let put = cas.put(b"x".to_vec()).await;
        assert!(
            matches!(&put, Err(CasError::Missing(m)) if m.contains("CAS call failed")),
            "{put:?}"
        );
    }
}
