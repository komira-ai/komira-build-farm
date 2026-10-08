//! `kbf-mdm-gate`'s flags and startup: read and check every file, reconcile with the
//! MDM, bind the mutual-TLS listener, print the start line, then serve and tick until
//! SIGTERM or SIGINT.
//!
//! Files the gate trusts (inventory, allowed signers, profile allowlist and profiles,
//! root key) must be owned by `--trusted-uid` (root unless testing) and writable by
//! nobody else. The two secrets (the NanoHUB API key and the grant key) must be
//! readable by their owner only.

use std::ffi::OsString;
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use crate::clock::SystemClock;
use crate::gate::{Files, Gate, Inventory, Parts, Policy};
use crate::grant::GrantKey;
use crate::journal::{Journal, LogAlerts};
use crate::nanohub::NanoHub;
use crate::state::StateFile;
use crate::trusted;

/// A startup error, for the operator.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// How long a client may take to finish its TLS handshake.
const HANDSHAKE: Duration = Duration::from_secs(10);

/// `kbf-mdm-gate`: the only holder of the MDM's API key, serving `kbf-server` the
/// verbs of the fleet-updates design over mutual TLS.
#[derive(Clone, Debug, Parser)]
#[command(name = "kbf-mdm-gate", version)]
pub struct Cli {
    /// Where to listen: the interface `kbf-server` reaches, never the Macs' network.
    #[arg(long)]
    pub listen: SocketAddr,
    /// The gate's TLS certificate chain (PEM).
    #[arg(long)]
    pub tls_cert: PathBuf,
    /// The gate's TLS private key (PEM).
    #[arg(long)]
    pub tls_key: PathBuf,
    /// The SHA-256 (hex) of `kbf-server`'s client SubjectPublicKeyInfo: the only client
    /// the gate accepts.
    #[arg(long)]
    pub server_key_sha256: String,
    /// NanoHUB's API, on loopback (for example `http://localhost:9004`).
    #[arg(long)]
    pub nanohub_url: String,
    /// A file holding NanoHUB's API key, readable by its owner only.
    #[arg(long)]
    pub nanohub_api_key_file: PathBuf,
    /// The inventory: `{"macs": [{"serial", "enrollment", "pool"}]}`.
    #[arg(long)]
    pub inventory: PathBuf,
    /// The operators' allowed-signers file, read again for every erase request.
    #[arg(long)]
    pub allowed_signers: PathBuf,
    /// Profile digests the host allows, one per line, read again for every install.
    #[arg(long)]
    pub profile_allowlist: PathBuf,
    /// Directory of `<sha256>.mobileconfig` profiles the gate may install.
    #[arg(long)]
    pub profile_dir: PathBuf,
    /// The offline root key's public half (32 bytes, base64), which signs key statements.
    #[arg(long)]
    pub root_key: PathBuf,
    /// The gate's grant key (a 32-byte ed25519 seed, base64), readable by its owner only.
    #[arg(long)]
    pub grant_key: PathBuf,
    /// Where the gate keeps its state and audit log.
    #[arg(long)]
    pub state_dir: PathBuf,
    /// The fewest Macs that must stay available (not being erased or updated).
    #[arg(long)]
    pub mac_floor: usize,
    /// Erases sent or reserved in any 24 hours.
    #[arg(long, default_value_t = 2)]
    pub daily_erase_cap: usize,
    /// The longest privileged lease, in minutes: a granted Mac is erased this long after
    /// its grant.
    #[arg(long)]
    pub max_lease_minutes: u32,
    /// Require the security key's "user verified" flag (PIN or biometric) on erase
    /// requests, as well as "user present".
    #[arg(long)]
    pub require_user_verified: bool,
    /// Seconds between ticks (held-request expiry, scheduled erases, re-enrollment).
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
    pub tick_seconds: u64,
    /// The uid that must own the trusted files. 0 (root) unless testing.
    #[arg(long, default_value_t = 0)]
    pub trusted_uid: u32,
}

/// Reads a secret: a file only its owner can read.
fn read_secret(path: &Path) -> Result<String, String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let meta = std::fs::metadata(path).map_err(|e| fail(&e))?;
    if meta.mode() & 0o077 != 0 {
        return Err(fail(&format!(
            "mode {:o}: a secret must be readable by its owner only",
            meta.mode() & 0o777
        )));
    }
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .map_err(|e| fail(&e))
}

fn parse_pin(text: &str) -> Result<[u8; 32], String> {
    hex::decode(text)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| "--server-key-sha256: expected 64 hex digits".to_owned())
}

/// Builds the gate from the flags' files.
///
/// # Errors
/// A file is missing, malformed or fails its ownership check.
pub fn gate(cli: &Cli) -> Result<Gate<NanoHub>, String> {
    let owner = cli.trusted_uid;
    let inventory = Inventory::parse(&trusted::read_text(&cli.inventory, owner)?)
        .map_err(|e| format!("{}: {e}", cli.inventory.display()))?;
    let root_key = crate::sets::parse_root_key(&trusted::read_text(&cli.root_key, owner)?)
        .ok_or_else(|| {
            format!(
                "{}: expected a base64 ed25519 public key",
                cli.root_key.display()
            )
        })?;
    let grant_key = GrantKey::parse(&read_secret(&cli.grant_key)?).ok_or_else(|| {
        format!(
            "{}: expected a base64 32-byte seed",
            cli.grant_key.display()
        )
    })?;
    // Checked now so a bad file stops the start, and read again on every use.
    let signers = trusted::read_text(&cli.allowed_signers, owner)?;
    crate::signers::AllowedSigners::parse(&signers).map_err(|e| e.to_string())?;
    trusted::read_text(&cli.profile_allowlist, owner)?;
    let backend = NanoHub::new(&cli.nanohub_url, read_secret(&cli.nanohub_api_key_file)?);
    std::fs::create_dir_all(&cli.state_dir)
        .map_err(|e| format!("{}: {e}", cli.state_dir.display()))?;
    let journal = Journal::open(&cli.state_dir.join("audit.log"), Box::new(LogAlerts))
        .map_err(|e| format!("audit log: {e}"))?;
    Gate::new(Parts {
        backend,
        clock: Arc::new(SystemClock),
        policy: Policy {
            mac_floor: cli.mac_floor,
            daily_erase_cap: cli.daily_erase_cap,
            max_lease_secs: i64::from(cli.max_lease_minutes) * 60,
            require_user_verified: cli.require_user_verified,
        },
        inventory,
        files: Files {
            allowed_signers: cli.allowed_signers.clone(),
            profile_allowlist: cli.profile_allowlist.clone(),
            profile_dir: cli.profile_dir.clone(),
            owner,
        },
        root_key,
        grant_key,
        journal,
        store: StateFile::new(&cli.state_dir.join("state.json")),
    })
    .map_err(|e| e.to_string())
}

/// Starts the gate and serves until `shutdown` completes. `ready` receives the bound
/// address once the start line is printed.
///
/// # Errors
/// Startup fails: a file, the MDM, the TLS identity or the listener.
pub async fn run(
    cli: Cli,
    shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
    ready: Option<tokio::sync::oneshot::Sender<SocketAddr>>,
) -> Result<(), Error> {
    let gate = Arc::new(gate(&cli)?);
    let withdrawn = gate
        .reconcile()
        .await
        .map_err(|e| format!("startup reconciliation: {e}"))?;
    tracing::info!(?withdrawn, "reconciled with the MDM");
    let (chain, key) = crate::tls::load_identity(&cli.tls_cert, &cli.tls_key)?;
    let config = crate::tls::server_config(chain, key, parse_pin(&cli.server_key_sha256)?)
        .map_err(|e| format!("TLS: {e}"))?;
    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .map_err(|e| format!("bind {}: {e}", cli.listen))?;
    let addr = listener.local_addr()?;
    println!("kbf-mdm-gate {} listen={addr}", env!("CARGO_PKG_VERSION"));
    if let Some(ready) = ready {
        let _ = ready.send(addr);
    }
    let ticker = Arc::clone(&gate);
    let every = Duration::from_secs(cli.tick_seconds);
    let ticking = tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        loop {
            interval.tick().await;
            let _ = ticker
                .tick()
                .await
                .inspect_err(|e| tracing::error!("tick: {e}"));
        }
    });
    let listener = Arc::new(listener);
    let serving = tokio::spawn(crate::tls::serve(
        move || {
            let listener = Arc::clone(&listener);
            async move { listener.accept().await.map(|(stream, _)| stream) }
        },
        config,
        crate::api::router(gate),
        HANDSHAKE,
    ));
    shutdown.await;
    ticking.abort();
    serving.abort();
    Ok(())
}

/// Installs the SIGTERM and SIGINT handlers now, and returns a future that completes
/// on either.
///
/// # Errors
/// A handler cannot be installed.
pub fn terminated() -> std::io::Result<impl Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    Ok(async move {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    })
}

/// The binary: parse `args`, start, serve until SIGTERM or SIGINT. Exit 0 on a clean
/// stop, 2 on bad flags or a failed start.
pub fn main(args: Vec<OsString>) -> ExitCode {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(2);
        }
    };
    let result = tokio::runtime::Runtime::new()
        .map_err(Error::from)
        .and_then(|runtime| {
            runtime.block_on(async { run(cli, Box::pin(terminated()?), None).await })
        });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-mdm-gate: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
