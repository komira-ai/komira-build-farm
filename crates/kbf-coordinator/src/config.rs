//! `kbf-coordinator`'s flags and start-up: read every file, bind both listeners, print the
//! start line, then serve until SIGTERM or SIGINT.
//!
//! Everything is a flag; the coordinator reads no environment variable. It holds no CA key:
//! its certificate and key are files the farm CA issued, and the CA certificate it checks
//! updaters against is a file too.

use std::convert::Infallible;
use std::ffi::OsString;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use kbf_oidc::{HttpsKeys, OidcConfig, Roots, Verifier};
use kbf_tls::{TlsFiles, TlsSource};
use tokio::net::TcpListener;

use crate::desired::DesiredStore;
use crate::http::Surface;
use crate::listen::{CLIENT_PATIENCE, Listener, MAX_CONNECTIONS, reload_every, serve};
use crate::service::{Clock, Coordinator};

/// A start-up error, for the operator.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// How long the verifier waits before it fetches the issuer's keys again for an unknown `kid`.
const KEY_REFETCH: Duration = Duration::from_secs(30);

/// How long fetching the issuer's discovery document or keys may take.
const KEY_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// `kbf-coordinator`: tells each host's updater which version to run, within per-role limits.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "kbf-coordinator",
    version = concat!(env!("CARGO_PKG_VERSION"), "+", env!("KBF_BUILD_COMMIT"))
)]
pub struct Cli {
    /// Where updaters dial (mutual TLS).
    #[arg(long)]
    pub heartbeat_listen: SocketAddr,
    /// Where CI sets the desired state and an operator reads the status (TLS).
    #[arg(long)]
    pub control_listen: SocketAddr,
    /// The coordinator's certificate chain (PEM), issued by the farm CA with the role
    /// `coordinator`.
    #[arg(long)]
    pub tls_cert: PathBuf,
    /// The coordinator's private key (PEM).
    #[arg(long)]
    pub tls_key: PathBuf,
    /// The farm CA certificate or bundle (PEM) an updater's certificate must chain to.
    #[arg(long)]
    pub client_ca: PathBuf,
    /// The desired-state file: written by the coordinator, read back at start. Its
    /// directory must be writable by the coordinator.
    #[arg(long)]
    pub state_file: PathBuf,
    /// The allow-list of CI's OIDC token: a TOML file with a `[remote_operator]` table.
    #[arg(long)]
    pub oidc_config: PathBuf,
    /// A file holding the operator token, mode 0600, owned by the coordinator's user.
    #[arg(long)]
    pub operator_token_file: PathBuf,
    /// How often the TLS files are looked at again, in seconds.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    pub tls_reload_secs: u64,
}

/// Where the listeners are bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bound {
    /// The heartbeat listener.
    pub heartbeat: SocketAddr,
    /// The control listener.
    pub control: SocketAddr,
}

/// What a running coordinator is made of.
pub struct Parts {
    /// The coordinator.
    pub coordinator: Arc<Coordinator>,
    /// The heartbeat listener's TLS (client certificates required).
    pub heartbeat_tls: TlsSource,
    /// The control listener's TLS.
    pub control_tls: TlsSource,
    /// Where to bind the heartbeat listener.
    pub heartbeat_listen: SocketAddr,
    /// Where to bind the control listener.
    pub control_listen: SocketAddr,
    /// How often to look at the TLS files.
    pub reload: Duration,
}

/// Reads every file the flags name and builds what runs. Fetches nothing: the issuer's keys
/// are fetched on the first token.
///
/// # Errors
/// A file is missing, unreadable or refused (the text names the flag or the file).
pub fn build(cli: &Cli) -> Result<Parts, Error> {
    let operator_token = kbf_updater::secret::read_token(&cli.operator_token_file)
        .map_err(|why| format!("--operator-token-file: {why}"))?;
    let oidc = OidcConfig::from_file(&cli.oidc_config)?;
    let keys = HttpsKeys::new(&oidc.issuer, Roots::System, KEY_FETCH_TIMEOUT)?;
    let verifier = Verifier::new(oidc, Arc::new(keys), KEY_REFETCH, kbf_oidc::system_clock());
    let desired = DesiredStore::open(&cli.state_file)?;
    let interval = Duration::from_secs(cli.tls_reload_secs);
    let files = |client_ca| TlsFiles {
        cert: cli.tls_cert.clone(),
        key: cli.tls_key.clone(),
        client_ca,
    };
    let origin = Instant::now();
    let clock: Clock = Arc::new(move || origin.elapsed());
    Ok(Parts {
        coordinator: Arc::new(Coordinator::new(
            desired,
            Arc::new(verifier),
            operator_token,
            clock,
        )),
        heartbeat_tls: TlsSource::files("heartbeat", files(Some(cli.client_ca.clone())), interval)?,
        control_tls: TlsSource::files("control", files(None), interval)?,
        heartbeat_listen: cli.heartbeat_listen,
        control_listen: cli.control_listen,
        reload: interval,
    })
}

/// Binds both listeners, calls `ready` with their addresses, and serves until the future is
/// dropped. It never returns `Ok`.
///
/// # Errors
/// An address cannot be bound.
pub async fn run(parts: Parts, ready: impl FnOnce(Bound)) -> Result<Infallible, Error> {
    let heartbeat = Listener {
        tcp: TcpListener::bind(parts.heartbeat_listen).await?,
        tls: parts.heartbeat_tls.clone(),
        surface: Surface::Heartbeat,
    };
    let control = Listener {
        tcp: TcpListener::bind(parts.control_listen).await?,
        tls: parts.control_tls.clone(),
        surface: Surface::Control,
    };
    ready(Bound {
        heartbeat: heartbeat.local_addr()?,
        control: control.local_addr()?,
    });
    let coordinator = parts.coordinator;
    let (never, _, _) = tokio::join!(
        serve(
            heartbeat,
            Arc::clone(&coordinator),
            CLIENT_PATIENCE,
            MAX_CONNECTIONS
        ),
        serve(control, coordinator, CLIENT_PATIENCE, MAX_CONNECTIONS),
        reload_every(vec![parts.heartbeat_tls, parts.control_tls], parts.reload),
    );
    match never {}
}

/// Resolves on SIGTERM or SIGINT.
async fn signalled() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    Ok(())
}

/// Runs the coordinator until `stop` resolves.
async fn start(cli: Cli, stop: impl Future<Output = std::io::Result<()>>) -> Result<(), Error> {
    let parts = build(&cli)?;
    let serving = run(parts, |bound| {
        println!(
            "kbf-coordinator {} heartbeat={} control={}",
            env!("CARGO_PKG_VERSION"),
            bound.heartbeat,
            bound.control
        );
    });
    tokio::select! {
        stopped = stop => stopped.map_err(Error::from),
        served = serving => {
            let Err(why) = served;
            Err(why)
        }
    }
}

/// Builds the runtime and runs the coordinator on it until a signal.
fn runtime(cli: Cli) -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(start(cli, signalled()))
}

/// The binary: flags in, exit code out. Bad flags are exit 2; a failed start is exit 1;
/// SIGTERM or SIGINT is exit 0.
#[must_use]
pub fn main(args: Vec<OsString>) -> ExitCode {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(usage) => {
            let _ = usage.print();
            return ExitCode::from(if usage.use_stderr() { 2 } else { 0 });
        }
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    match runtime(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("kbf-coordinator: {why}");
            ExitCode::from(1)
        }
    }
}
