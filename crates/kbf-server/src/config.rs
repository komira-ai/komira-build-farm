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

use kbf_auth::{Policy, PolicyError};

use crate::identity::{DenyList, DenyListError};
use crate::serve::{Api, Listeners, WorkerTls};
use crate::token::{ApiToken, TokenFileError};

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
#[command(
    name = "kbf-server",
    version = crate::SERVER_VERSION,
    about = "The kbf farm server"
)]
pub struct Args {
    /// The roles to run.
    #[arg(long, value_enum, default_value = "all")]
    pub role: Role,
    /// Where blob bytes are stored.
    #[arg(long, value_enum, default_value = "memory")]
    pub store: StoreKind,
    /// The REAPI listener. Without `--reapi-tls-cert` and `--reapi-tls-key` it serves
    /// plain text with no authentication: bind it to loopback only.
    #[arg(long, default_value = "127.0.0.1:8980")]
    pub listen: SocketAddr,
    /// PEM certificate (chain) of the REAPI listener. With `--reapi-tls-key` it serves
    /// TLS; clients present no certificate. A daemon whose `--cas` is an `https://` URL
    /// of this listener checks it with its own CA and `--tls-server-name` (the URL's
    /// host without it), so the certificate must chain to that CA and carry that name;
    /// the worker listener's certificate and key may be given here when its names
    /// cover the REAPI address clients dial.
    #[arg(long, requires = "reapi_tls_key")]
    pub reapi_tls_cert: Option<PathBuf>,
    /// PEM private key of the REAPI listener; it must match `--reapi-tls-cert`, or the
    /// server refuses to start.
    #[arg(long, requires = "reapi_tls_cert")]
    pub reapi_tls_key: Option<PathBuf>,
    /// A JSON file with the REAPI listener's authentication policy and authorizers
    /// (`docs/reapi-auth.md`), read once at start; a file that is not a valid policy
    /// stops the server. Without it every REAPI call is accepted and allowed. The
    /// worker listener and the operator API do not read it.
    #[arg(long)]
    pub reapi_auth_policy: Option<PathBuf>,
    /// The `kbf.worker.v1` listener.
    #[arg(long, default_value = "127.0.0.1:8981")]
    pub worker_listen: SocketAddr,
    /// The operator API listener (HTTP/JSON under `/v1`). Off unless given. Reads are
    /// open to whoever reaches it: bind it where only operators do.
    #[arg(long)]
    pub api_listen: Option<SocketAddr>,
    /// A file holding the token operator API writes (cordon, drain, uncordon) must
    /// present as `Authorization: Bearer <token>`. It must be owned by the server's
    /// user with mode 0600 or 0400, or the server refuses to start; run the server as
    /// a user other than the daemon's, whose builds could read it. Without it, writes
    /// are refused.
    #[arg(long, requires = "api_listen")]
    pub api_token_file: Option<PathBuf>,
    /// How long, in milliseconds, `GET /readyz` on the operator API waits for the
    /// object store to answer its read-only probe before it answers 503.
    #[arg(
        long,
        default_value_t = crate::health::STORE_PROBE_TIMEOUT.as_millis() as u64,
        value_parser = clap::value_parser!(u64).range(1..=60_000)
    )]
    pub readyz_store_timeout_ms: u64,
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
    /// The worker listener's deny list: certificate serials, public keys and node ids
    /// it refuses (format in the `identity` module docs). Read at start, then again at
    /// every `Hello` and `Heartbeat`, so an edit takes effect without a restart. Needs
    /// mutual TLS.
    #[arg(long, requires_all = ["worker_tls_cert", "worker_tls_key", "worker_client_ca"])]
    pub worker_deny_list: Option<PathBuf>,
    /// The heartbeat interval daemons are asked for, in milliseconds, at most half the
    /// window in which a daemon may act on a `Start` (each `Start` names the newest
    /// heartbeat the server took, so a longer interval would leave it too little).
    #[arg(
        long,
        default_value_t = 5_000,
        value_parser = clap::value_parser!(u64).range(1..=MAX_HEARTBEAT_INTERVAL_MS)
    )]
    pub heartbeat_interval_ms: u64,
    /// How long, in seconds, queued work waits while no connected daemon can run it
    /// (none satisfies its platform, or none that does is large enough) before it is
    /// refused FAILED_PRECONDITION. The wait restarts whenever one can.
    #[arg(long, default_value_t = kbf_sched::UNSERVABLE_WAIT.as_secs())]
    pub unservable_wait_secs: u64,
    /// How long, in seconds, a finished operation is kept after its callers are
    /// answered. Within it, WaitExecution on the operation's name streams its result
    /// (a client whose Execute stream broke gets a result the action cache does not
    /// keep, such as a failure); after it, the name is NOT_FOUND. Every operation
    /// finished within it is held in memory.
    #[arg(long, default_value_t = kbf_sched::FINISHED_RETENTION.as_secs())]
    pub finished_retention_secs: u64,
    /// How long, in seconds, a stop (SIGINT or SIGTERM) waits for REAPI clients to
    /// disconnect after their open Execute and WaitExecution streams are ended
    /// UNAVAILABLE and their connections sent GOAWAY. A client still uploading is cut
    /// off when it runs out.
    #[arg(long, default_value_t = 10)]
    pub shutdown_timeout_secs: u64,
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
    /// The deny list cannot be read or parsed.
    #[error("--worker-deny-list: {0}")]
    DenyList(#[from] DenyListError),
    /// An S3 setting is missing from the environment.
    #[error("{0} must be set for --store=s3")]
    MissingEnv(&'static str),
    /// The S3 configuration is refused.
    #[error(transparent)]
    S3(#[from] S3ConfigError),
    /// The key prefix is not a valid prefix.
    #[error("--s3-prefix: {0}")]
    Prefix(#[from] KeyError),
    /// The operator API token file is refused.
    #[error("--api-token-file: {0}")]
    ApiToken(#[from] TokenFileError),
    /// The REAPI policy file cannot be read.
    #[error("--reapi-auth-policy: read {path}: {source}")]
    PolicyRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The REAPI policy file is not a policy.
    #[error("--reapi-auth-policy: {path}: {source}")]
    Policy {
        path: PathBuf,
        #[source]
        source: PolicyError,
    },
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
            (Some(cert), Some(key), Some(ca)) => Some(WorkerTls {
                server: ServerTlsConfig::new()
                    .identity(Identity::from_pem(read(cert)?, read(key)?))
                    .client_ca_root(Certificate::from_pem(read(ca)?)),
                deny_list: self
                    .worker_deny_list
                    .as_deref()
                    .map(DenyList::open)
                    .transpose()?,
            }),
            _ => None,
        };
        let reapi_tls = match (&self.reapi_tls_cert, &self.reapi_tls_key) {
            (Some(cert), Some(key)) => {
                Some(ServerTlsConfig::new().identity(Identity::from_pem(read(cert)?, read(key)?)))
            }
            _ => None,
        };
        Ok(Listeners {
            reapi: self.listen,
            reapi_tls,
            worker: self.worker_listen,
            worker_tls,
            heartbeat_interval: Duration::from_millis(self.heartbeat_interval_ms),
            hello_wait: Duration::from_secs(10),
            tick: Duration::from_secs(1),
            unservable_wait: Duration::from_secs(self.unservable_wait_secs),
            finished_retention: Duration::from_secs(self.finished_retention_secs),
            shutdown_timeout: Duration::from_secs(self.shutdown_timeout_secs),
        })
    }

    /// The REAPI listener's policy: the file `--reapi-auth-policy` names, or
    /// [`Policy::allow_all`] without it.
    ///
    /// # Errors
    /// The file cannot be read, or is not a policy ([`Policy::from_json`]).
    pub fn reapi_auth_policy(&self) -> Result<Policy, ConfigError> {
        let Some(path) = &self.reapi_auth_policy else {
            return Ok(Policy::allow_all());
        };
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::PolicyRead {
            path: path.clone(),
            source,
        })?;
        Policy::from_json(&text).map_err(|source| ConfigError::Policy {
            path: path.clone(),
            source,
        })
    }

    /// The operator API these flags describe, if `--api-listen` is given, with the
    /// token read from `--api-token-file`.
    ///
    /// # Errors
    /// The token file is refused (see [`ApiToken::from_file`]).
    pub fn api(&self) -> Result<Option<Api>, ConfigError> {
        let Some(listen) = self.api_listen else {
            return Ok(None);
        };
        let token = self
            .api_token_file
            .as_deref()
            .map(ApiToken::from_file)
            .transpose()?;
        Ok(Some(Api {
            listen,
            token,
            store_probe_timeout: Duration::from_millis(self.readyz_store_timeout_ms),
        }))
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
