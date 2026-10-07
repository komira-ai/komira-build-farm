//! Configuration: command-line flags, and the TLS material they name.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tonic::transport::{Certificate, ClientTlsConfig, Identity};

/// T: a daemon fences its leases this long after the newest acknowledged heartbeat
/// (RFC 5.8). The scheduler re-dispatches after G = 60 s; safety needs T + 5 s < G.
pub const FENCE_AFTER: Duration = Duration::from_secs(40);

/// The `kbf-daemon` command line.
#[derive(Clone, Debug, clap::Parser)]
#[command(name = "kbf-daemon", version, about = "The kbf worker daemon.")]
pub struct Args {
    /// The kbf-server front to connect to, as an `https://host:port` URL.
    #[arg(long)]
    pub server: String,
    /// PEM file of the CA certificates that sign server certificates.
    #[arg(long)]
    pub ca_cert: PathBuf,
    /// PEM file of this daemon's client certificate chain.
    #[arg(long)]
    pub cert: PathBuf,
    /// PEM file of this daemon's private key.
    #[arg(long)]
    pub key: PathBuf,
    /// The name to verify in the server certificate, when it is not the URL's host.
    #[arg(long)]
    pub tls_server_name: Option<String>,
    /// A stable name for this node, unique within the cell.
    #[arg(long)]
    pub node_id: String,
    /// The execution runtime.
    #[arg(long, value_enum)]
    pub runtime: RuntimeKind,
    /// How long to wait between connection attempts, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    pub reconnect_ms: u64,
}

/// The runtimes this build can run leases through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum RuntimeKind {
    /// Runs nothing; every action succeeds with an empty result. For bring-up only.
    Fake,
}

/// The TLS files a daemon authenticates with: the server's CA, and its own
/// certificate and key.
#[derive(Clone, Debug)]
pub struct TlsFiles {
    pub ca_cert: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub server_name: Option<String>,
}

/// Why configuration could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("server URL {url:?}: {source}")]
    Server {
        url: String,
        #[source]
        source: tonic::transport::Error,
    },
    #[error("server URL {0:?} is not https; the daemon connects only over mutual TLS")]
    NotHttps(String),
}

impl TlsFiles {
    /// Reads the files into a client TLS configuration that presents the daemon's
    /// certificate and trusts only the given CA.
    pub fn load(&self) -> Result<ClientTlsConfig, ConfigError> {
        let ca = Certificate::from_pem(read(&self.ca_cert)?);
        let identity = Identity::from_pem(read(&self.cert)?, read(&self.key)?);
        let mut tls = ClientTlsConfig::new().ca_certificate(ca).identity(identity);
        if let Some(name) = &self.server_name {
            tls = tls.domain_name(name.clone());
        }
        Ok(tls)
    }
}

fn read(path: &Path) -> Result<Vec<u8>, ConfigError> {
    std::fs::read(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })
}

/// Everything a [`crate::Daemon`] needs besides its runtime and node report.
#[derive(Clone, Debug)]
pub struct DaemonConfig {
    /// The server front, an `https` URL.
    pub server: String,
    pub tls: TlsFiles,
    pub node_id: String,
    /// T, normally [`FENCE_AFTER`]. Tests shorten it.
    pub fence_after: Duration,
    /// The wait between connection attempts.
    pub reconnect_after: Duration,
    /// How long to wait for Welcome after opening a stream.
    pub welcome_timeout: Duration,
}

impl DaemonConfig {
    /// A configuration with the production fence time.
    #[must_use]
    pub fn new(server: String, tls: TlsFiles, node_id: String) -> Self {
        Self {
            server,
            tls,
            node_id,
            fence_after: FENCE_AFTER,
            reconnect_after: Duration::from_secs(1),
            welcome_timeout: Duration::from_secs(10),
        }
    }

    /// The configuration the command line describes.
    #[must_use]
    pub fn from_args(args: &Args) -> Self {
        let tls = TlsFiles {
            ca_cert: args.ca_cert.clone(),
            cert: args.cert.clone(),
            key: args.key.clone(),
            server_name: args.tls_server_name.clone(),
        };
        let mut config = Self::new(args.server.clone(), tls, args.node_id.clone());
        config.reconnect_after = Duration::from_millis(args.reconnect_ms);
        config
    }
}
