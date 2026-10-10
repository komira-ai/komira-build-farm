//! The integration cell's daemon: the `kbf-daemon` library's session loop with its
//! [`LocalRuntime`], which runs each action as a plain child process. **Test cells
//! only.**
//!
//! The `kbf-daemon` binary deliberately does not offer the local runtime, because it
//! isolates nothing (see `kbf_daemon::LocalRuntime`): an action runs as the daemon's
//! user, with its filesystem and network. Farm nodes run actions through the container
//! driver. Until that driver is merged, the M1 harness runs this daemon instead; what
//! it proves is the protocol and the cache path (Start, input fetch, output upload,
//! the result, the action-cache write), not isolation.
//!
//! Everything else is the production daemon: the same `Daemon`, mutual TLS to the
//! worker listener for the session and for blobs (`--cas`), the node report detected
//! from this machine, and the fence time.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use kbf_daemon::config::ConfigError;
use kbf_daemon::report::DetectError;
use kbf_daemon::{
    CasClient, Daemon, DaemonConfig, LOCAL_DRIVER, LocalRuntime, NodeReport, TlsFiles,
};
use tonic::transport::Endpoint;

/// `kbf-cell daemon` flags.
#[derive(Clone, Debug, clap::Args)]
pub struct DaemonArgs {
    /// The server's worker listener, an `https://host:port` URL.
    #[arg(long)]
    pub server: String,
    /// The server's worker listener again, an `https://host:port` URL: the daemon
    /// reads inputs and writes outputs through its `ByteStream`, over mutual TLS with
    /// the same files as the session. Plain `http://` is refused.
    #[arg(long)]
    pub cas: String,
    /// PEM file of the CA that signed the server certificate.
    #[arg(long)]
    pub ca_cert: PathBuf,
    /// PEM file of the daemon's client certificate.
    #[arg(long)]
    pub cert: PathBuf,
    /// PEM file of the daemon's private key.
    #[arg(long)]
    pub key: PathBuf,
    /// The name to verify in the server certificate.
    #[arg(long, default_value = crate::pki::SERVER_NAME)]
    pub tls_server_name: String,
    /// A stable name for this node.
    #[arg(long, default_value = "cell-node-1")]
    pub node_id: String,
    /// Where each lease gets its directory.
    #[arg(long)]
    pub scratch: PathBuf,
}

/// Why the daemon could not start.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("--cas {url:?}: {source}")]
    Cas {
        url: String,
        #[source]
        source: tonic::transport::Error,
    },
    #[error(
        "--cas {0:?} must be an https:// URL: the server's worker listener, which serves \
         blobs over mutual TLS only"
    )]
    CasNotHttps(String),
    #[error("detect the node: {0}")]
    Detect(#[from] DetectError),
    #[error(transparent)]
    Config(#[from] ConfigError),
}

/// Runs the daemon until `shutdown` completes.
///
/// # Errors
/// A flag is refused, a TLS file cannot be read, or the node cannot be detected.
pub async fn run(args: DaemonArgs, shutdown: impl Future<Output = ()>) -> Result<(), DaemonError> {
    if !args.cas.starts_with("https://") {
        return Err(DaemonError::CasNotHttps(args.cas));
    }
    let tls = TlsFiles {
        ca_cert: args.ca_cert,
        cert: args.cert,
        key: args.key,
        server_name: Some(args.tls_server_name),
    };
    let fail = |source| DaemonError::Cas {
        url: args.cas.clone(),
        source,
    };
    // Lazy: the daemon may start before the server, and the CAS is reached per lease.
    let channel = Endpoint::from_shared(args.cas.clone())
        .map_err(fail)?
        .tls_config(tls.load()?)
        .map_err(fail)?
        .connect_lazy();
    let runtime = Arc::new(LocalRuntime::new(
        Arc::new(CasClient::new(channel)),
        args.scratch,
    ));
    let report = NodeReport::detect(&[LOCAL_DRIVER])?;
    let config = DaemonConfig::new(args.server, tls, args.node_id);
    let daemon = Daemon::new(config, runtime, report)?;
    daemon.run(shutdown).await;
    Ok(())
}
