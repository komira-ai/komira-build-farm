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

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kbf_daemon::{Args, CasClient, Daemon, DaemonConfig, FakeRuntime, NodeReport, Runtime};
use kbf_driver_native::{MemoryPolicy, NativeConfig, NativeRuntime, xcode};
use kbf_outputs::OutputLimits;
use tokio::signal::unix::{Signal, SignalKind, signal};
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
    /// The directory searched for `Xcode*.app` (native). Each Xcode that answers
    /// `xcodebuild -version` is reported as an `xcode` entry, and an action that names
    /// its build runs with it as `DEVELOPER_DIR`.
    #[arg(long, default_value = xcode::APPLICATIONS)]
    xcode_apps: PathBuf,
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
    config.xcodes = xcode::discover(&cli.xcode_apps, Path::new(xcode::XCODEBUILD));
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

    use futures::stream;
    use kbf_daemon::cas::Chunks;
    use kbf_driver_container::{
        Cas, CasError, FileBlob, OutputLimits, PodmanConfig, PodmanRuntime,
    };
    use kbf_proto::reapi::Digest;

    use super::{Cli, Error, cas_client, scratch, serve};

    /// Builds the container driver, over the daemon's CAS client, and serves with it.
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
            max_stdio_bytes: cli.outputs.max_stdio_bytes,
        };
        let cas = ContainerCas(cas_client(cli)?);
        let runtime = PodmanRuntime::new(config, Arc::new(cas))?;
        serve(cli, tokio, Arc::new(runtime), [])
    }

    /// The daemon's CAS client behind the container driver's own `Cas` trait, until
    /// that driver moves onto the daemon's (a follow-up). A file's chunks go to
    /// `put_chunks` as the container driver's `FileBlob` reads them.
    pub(super) struct ContainerCas<C>(pub(super) C);

    impl<C: kbf_daemon::Cas> Cas for ContainerCas<C> {
        async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
            self.0.get(digest).await.map_err(convert)
        }

        async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
            self.0.put(bytes).await.map_err(convert)
        }

        async fn put_file(&self, blob: FileBlob) -> Result<Digest, CasError> {
            let digest = blob.digest().clone();
            let chunks: Chunks = Box::pin(stream::unfold(Some(blob), |state| async move {
                let mut blob = state?;
                match blob.next_chunk().await {
                    Ok(Some(chunk)) => Some((Ok(chunk), Some(blob))),
                    Ok(None) => None,
                    Err(e) => Some((Err(e), None)),
                }
            }));
            self.0.put_chunks(digest, chunks).await.map_err(convert)
        }
    }

    /// The daemon's CAS error as the container driver's. That driver has no "the CAS
    /// failed the call" (it fails the lease on any CAS error), so an outage is named
    /// in a missing-blob message; the other kinds keep their meaning.
    pub(super) fn convert(error: kbf_daemon::CasError) -> CasError {
        match error {
            kbf_daemon::CasError::Missing(blob) => CasError::Missing(blob),
            kbf_daemon::CasError::Corrupt(blob, actual) => CasError::Corrupt(blob, actual),
            kbf_daemon::CasError::Read(blob, why) => CasError::Read(blob, why),
            kbf_daemon::CasError::Unavailable(blob, why) => {
                CasError::Missing(format!("{blob} (the CAS call failed: {why})"))
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Catches a CAS outage reported to the container driver as a corrupt blob, or
        /// its reason lost; and the other kinds changing on the way.
        #[test]
        fn errors_keep_their_meaning() {
            use kbf_daemon::CasError as Daemon;
            let pairs = [
                (
                    Daemon::Missing("a/1".into()),
                    CasError::Missing("a/1".into()),
                ),
                (
                    Daemon::Corrupt("a/1".into(), "b/1".into()),
                    CasError::Corrupt("a/1".into(), "b/1".into()),
                ),
                (
                    Daemon::Read("a/1".into(), "eof".into()),
                    CasError::Read("a/1".into(), "eof".into()),
                ),
                (
                    Daemon::Unavailable("a/1".into(), "down".into()),
                    CasError::Missing("a/1 (the CAS call failed: down)".into()),
                ),
            ];
            for (daemon, container) in pairs {
                assert_eq!(convert(daemon), container);
            }
        }

        /// One blob in memory, stored through the daemon's default `put_chunks`.
        #[derive(Default)]
        struct OneBlob(std::sync::Mutex<Vec<u8>>);

        impl kbf_daemon::Cas for OneBlob {
            async fn get(&self, _: &Digest) -> Result<Vec<u8>, kbf_daemon::CasError> {
                Ok(self.0.lock().expect("lock").clone())
            }

            async fn put(&self, bytes: Vec<u8>) -> Result<Digest, kbf_daemon::CasError> {
                let digest = kbf_daemon::cas::digest_of(&bytes);
                *self.0.lock().expect("lock") = bytes;
                Ok(digest)
            }
        }

        /// Catches a file stored through the adapter with bytes lost or reordered
        /// across chunks, a get or put not reaching the daemon's CAS, and a file that
        /// shrank after it was hashed taken as stored rather than failing as a read.
        #[tokio::test]
        async fn files_go_through_in_chunks_and_a_shrunk_file_fails() {
            let dir = std::env::current_exe()
                .expect("test binary")
                .parent()
                .expect("deps")
                .join("kbf-node-unit");
            std::fs::create_dir_all(&dir).expect("mkdir");
            let path = dir.join(format!("blob-{}", std::process::id()));
            let bytes: Vec<u8> = (0..(2 * kbf_driver_container::CHUNK + 3))
                .map(|i| (i % 251) as u8)
                .collect();
            std::fs::write(&path, &bytes).expect("write");
            let cas = ContainerCas(OneBlob::default());
            let open = || std::fs::File::open(&path).expect("open");
            let blob = FileBlob::hash(open(), u64::MAX)
                .expect("hash")
                .expect("fits");
            let digest = cas.put_file(blob).await.expect("stored");
            assert_eq!(digest, kbf_daemon::cas::digest_of(&bytes));
            assert_eq!(cas.get(&digest).await.expect("get"), bytes);
            assert_eq!(cas.put(b"x".to_vec()).await.expect("put").size_bytes, 1);

            let blob = FileBlob::hash(open(), u64::MAX)
                .expect("hash")
                .expect("fits");
            std::fs::File::create(&path).expect("truncate");
            let error = cas.put_file(blob).await.expect_err("shrunk");
            assert_eq!(
                std::mem::discriminant(&error),
                std::mem::discriminant(&CasError::Read(String::new(), String::new())),
                "{error:?}"
            );
            std::fs::remove_file(&path).expect("remove");
        }

        /// Catches the daemon's client not reached by the adapter (an unreachable
        /// front must surface as the converted outage).
        #[tokio::test]
        async fn the_daemons_client_is_reached() {
            let channel =
                tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
            let cas = ContainerCas(kbf_daemon::CasClient::new(channel));
            let digest = kbf_daemon::cas::digest_of(b"x");
            let got = cas.get(&digest).await.expect_err("unreachable");
            assert!(got.to_string().contains("CAS call failed"), "{got}");
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
    /// memory limit, poll and output limits, where Xcodes are looked for), and a
    /// relative or missing scratch directory accepted.
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
        assert!(config.xcodes.is_empty(), "no Xcode under /var/kbf/apps");
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
