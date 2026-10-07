//! Command-line flags. Every setting is a flag; the only environment variables read are
//! the standard S3 key pair, `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (a secret
//! never goes on the command line).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};
use kbf_objstore::s3::{Credentials, S3Config, S3ConfigError, S3Store};
use kbf_objstore::{Capabilities, KeyError, KeyPrefix};
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

use crate::serve::Listeners;

/// The longest heartbeat interval `--heartbeat-interval-ms` accepts: half of
/// [`kbf_sched::fence::START_VALIDITY`].
pub const MAX_HEARTBEAT_INTERVAL_MS: u64 = kbf_sched::fence::START_VALIDITY.as_millis() as u64 / 2;

/// The roles a server can run. A single node runs them all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Role {
    /// Front, scheduler and metadata in one process.
    All,
}

/// Where blob bytes go.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StoreKind {
    /// In this process; gone when it exits.
    Memory,
    /// An S3 bucket (MinIO, RustFS). The index is still in memory, so a restart
    /// forgets every blob; each start writes under a fresh key prefix.
    S3,
}

/// `kbf-server` flags.
#[derive(Clone, Debug, Parser)]
#[command(name = "kbf-server", version, about = "The kbf farm server")]
pub struct Args {
    /// The roles to run.
    #[arg(long, value_enum, default_value = "all")]
    pub role: Role,
    /// Where blob bytes are stored.
    #[arg(long, value_enum, default_value = "memory")]
    pub store: StoreKind,
    /// The REAPI listener.
    #[arg(long, default_value = "127.0.0.1:8980")]
    pub listen: SocketAddr,
    /// The `kbf.worker.v1` listener.
    #[arg(long, default_value = "127.0.0.1:8981")]
    pub worker_listen: SocketAddr,
    /// PEM certificate of the worker listener. With `--worker-tls-key` and
    /// `--worker-client-ca` it serves mutual TLS; without all three, plain text.
    #[arg(long, requires_all = ["worker_tls_key", "worker_client_ca"])]
    pub worker_tls_cert: Option<PathBuf>,
    /// PEM private key of the worker listener.
    #[arg(long, requires_all = ["worker_tls_cert", "worker_client_ca"])]
    pub worker_tls_key: Option<PathBuf>,
    /// PEM CA that daemon client certificates must chain to.
    #[arg(long, requires_all = ["worker_tls_cert", "worker_tls_key"])]
    pub worker_client_ca: Option<PathBuf>,
    /// The heartbeat interval daemons are asked for, in milliseconds, at most half the
    /// window in which a daemon may act on a `Start` (each `Start` names the newest
    /// heartbeat the server took, so a longer interval would leave it too little).
    #[arg(
        long,
        default_value_t = 5_000,
        value_parser = clap::value_parser!(u64).range(1..=MAX_HEARTBEAT_INTERVAL_MS)
    )]
    pub heartbeat_interval_ms: u64,
    /// `http://host[:port]` of the S3 service (`--store=s3`).
    #[arg(long, required_if_eq("store", "s3"))]
    pub s3_endpoint: Option<String>,
    /// The bucket (`--store=s3`).
    #[arg(long, required_if_eq("store", "s3"))]
    pub s3_bucket: Option<String>,
    /// The signing region.
    #[arg(long, default_value = "us-east-1")]
    pub s3_region: String,
    /// The key prefix; each start writes under `<prefix><start time>/`.
    #[arg(long, default_value = "kbf/")]
    pub s3_prefix: String,
    /// The store refuses to overwrite a key (MinIO and RustFS do).
    #[arg(long)]
    pub s3_conditional_put: bool,
}

/// Why the flags do not make a working server.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A TLS file could not be read.
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// An S3 setting is missing from the environment.
    #[error("{0} must be set for --store=s3")]
    MissingEnv(&'static str),
    /// The S3 configuration is refused.
    #[error(transparent)]
    S3(#[from] S3ConfigError),
    /// The key prefix is not a valid prefix.
    #[error("--s3-prefix: {0}")]
    Prefix(#[from] KeyError),
}

impl Args {
    /// The listeners these flags describe.
    ///
    /// # Errors
    /// A TLS file cannot be read.
    pub fn listeners(&self) -> Result<Listeners, ConfigError> {
        let worker_tls = match (
            &self.worker_tls_cert,
            &self.worker_tls_key,
            &self.worker_client_ca,
        ) {
            (Some(cert), Some(key), Some(ca)) => Some(
                ServerTlsConfig::new()
                    .identity(Identity::from_pem(read(cert)?, read(key)?))
                    .client_ca_root(Certificate::from_pem(read(ca)?)),
            ),
            _ => None,
        };
        Ok(Listeners {
            reapi: self.listen,
            worker: self.worker_listen,
            worker_tls,
            heartbeat_interval: Duration::from_millis(self.heartbeat_interval_ms),
            hello_wait: Duration::from_secs(10),
            tick: Duration::from_secs(1),
        })
    }

    /// The S3 store and the key prefix of this start, for `--store=s3`. `env` reads
    /// an environment variable (the S3 key pair).
    ///
    /// # Errors
    /// A flag or the key pair is missing, or the configuration is refused.
    pub fn s3_store(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<(S3Store, KeyPrefix), ConfigError> {
        let need = |name: &'static str| env(name).ok_or(ConfigError::MissingEnv(name));
        let credentials =
            Credentials::new(need("AWS_ACCESS_KEY_ID")?, need("AWS_SECRET_ACCESS_KEY")?);
        // clap requires both flags with `--store=s3`; an empty value is S3Store's to refuse.
        let store = S3Store::new(S3Config {
            endpoint: self.s3_endpoint.clone().unwrap_or_default(),
            region: self.s3_region.clone(),
            bucket: self.s3_bucket.clone().unwrap_or_default(),
            credentials,
            capabilities: Capabilities {
                conditional_put: self.s3_conditional_put,
                object_lock: false,
            },
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
        })?;
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let prefix = KeyPrefix::new(format!("{}{started}/", self.s3_prefix))?;
        Ok((store, prefix))
    }
}

fn read(path: &PathBuf) -> Result<Vec<u8>, ConfigError> {
    std::fs::read(path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })
}
