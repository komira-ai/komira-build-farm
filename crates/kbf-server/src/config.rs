//! Command-line flags. Every setting is a flag; the only environment variables read are
//! the standard S3 key pair, `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (a secret
//! never goes on the command line).

use std::net::{IpAddr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};
use kbf_front::WriteConfig;
use kbf_objstore::s3::{Credentials, S3Config, S3ConfigError, S3Store};
use kbf_objstore::{Capabilities, KeyError, KeyPrefix};
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

use kbf_auth::{AllowAuthorizer, Authorizer, Policy, PolicyError};
use kbf_front::SegmentsConfig;
use kbf_meta::ServerId;
use kbf_tier::{TierArgs, TierOptions};

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
    /// An S3 bucket (MinIO, RustFS). Under `--meta-log=memory` the index is in
    /// memory, so a restart forgets every blob, and each start writes under a fresh
    /// key prefix; under `--meta-log=raft` a restart keeps them.
    S3,
}

/// Where the metadata (the CAS index and the action cache) is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum MetaLogKind {
    /// In this process; a restart forgets it.
    Memory,
    /// A single-voter Raft log in `--raft-dir` ([`crate::raft_log`]); a restart keeps
    /// it. Needs `--store=s3`: the bytes it indexes must outlive the process too.
    Raft,
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
    /// Where the metadata (the CAS index and the action cache) is kept.
    #[arg(long, value_enum, default_value = "memory")]
    pub meta_log: MetaLogKind,
    /// The directory of the Raft log (`--meta-log=raft`), on a local disk; created if
    /// missing. An empty directory gets a new log with a new id; every later start
    /// over it reads the id from the directory.
    #[arg(long, required_if_eq("meta_log", "raft"))]
    pub raft_dir: Option<PathBuf>,
    /// The REAPI listener. Without `--reapi-tls-cert` and `--reapi-tls-key` it serves
    /// plain text, on any address; off loopback the start line carries a warning.
    #[arg(long, default_value = "127.0.0.1:8980")]
    pub listen: SocketAddr,
    /// PEM certificate (chain) of the REAPI listener. With `--reapi-tls-key` it serves
    /// TLS with this server certificate only; clients present no certificate. It is meant
    /// for a certificate from the farm's own internal CA, trusted by the front's proxy
    /// (its hop to this server) and by clients that dial this server directly; TLS for
    /// the farm's client-facing name still ends at the front. `--reapi-auth-policy`
    /// applies over TLS as over plain text.
    #[arg(long, requires = "reapi_tls_key")]
    pub reapi_tls_cert: Option<PathBuf>,
    /// PEM private key of the REAPI listener; it must match `--reapi-tls-cert`, or the
    /// server refuses to start.
    #[arg(long, requires = "reapi_tls_cert")]
    pub reapi_tls_key: Option<PathBuf>,
    /// A JSON file with the REAPI listener's authentication policy and authorizers
    /// (`docs/reapi-auth.md`), read once at start; a file that is not a valid policy
    /// stops the server. Without it every REAPI call is accepted and allowed. The
    /// worker listener does not read it; on the operator API only the build pages and
    /// the blob route do, and they are off unless it names their authorizers.
    #[arg(long)]
    pub reapi_auth_policy: Option<PathBuf>,
    /// The `kbf.worker.v1` listener. Without mutual TLS (`--worker-tls-cert`,
    /// `--worker-tls-key`, `--worker-client-ca`) it serves plain text, on any address;
    /// off loopback the start line carries a warning.
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
    /// Serves the build pages (`GET /v1/invocations/{id}` and below) and the blob route
    /// (`GET /v1/blobs/{digest}`) on the operator API to every caller the REAPI policy
    /// accepts, with no `invocations` or `blobs` key in the policy file
    /// (`docs/design/invocations.md` section 4.3): it fills in `allow` for whichever of
    /// the two keys the file leaves out. Without it, and without the keys, those routes
    /// answer 404. Pages and blobs hold build outputs and logs: give it only for a
    /// listener that only trusted people reach.
    #[arg(long)]
    pub invocations_open: bool,
    /// How long, in milliseconds, `GET /readyz` on the operator API and the REAPI
    /// listener's `grpc.health.v1` service wait for the object store to answer their
    /// read-only probe before they answer not ready.
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
    /// The memory, in bytes, the invocation records may take together
    /// (`docs/design/invocations.md`, section 3.5). At the bound, records of a minted id
    /// go first, then idle ones, least recently seen first; with only open builds left,
    /// the largest stops taking rows. Records are kept in memory only and lost on
    /// restart. Their heap was measured at up to 1.08 times this at 200 kB and above,
    /// and 1.12 at 50 kB (section 3.5).
    #[arg(
        long,
        default_value_t = kbf_front::DEFAULT_INVOCATION_MEMORY as u64,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub invocation_memory: u64,
    /// The most bytes, records and footers together, one segment upload on the worker
    /// listener may declare (`docs/design/write-path.md`, section 4.2). Ranges are held
    /// in memory until their upload commits, admitted under `--write-bytes-in-flight`
    /// with every other upload: a `Begin` waits while that is spent. At least one 1 GiB
    /// blob and its footer (1 GiB + 1 MiB), so any blob the cache accepts can be
    /// uploaded, and at most `--write-bytes-in-flight`, so any legal `Begin` can be
    /// admitted; the server refuses to start otherwise. At most 4 GiB - 1
    /// (`kbf_front::MAX_ADMISSION_BYTES`), the most one admission counts, so a `Begin`
    /// never holds more than it is counted for under the bound.
    #[arg(
        long,
        value_name = "BYTES",
        default_value_t = kbf_front::MIN_SEGMENT_UPLOAD_BYTES,
        value_parser = clap::value_parser!(u64)
            .range(kbf_front::MIN_SEGMENT_UPLOAD_BYTES..=kbf_front::MAX_ADMISSION_BYTES)
    )]
    pub segment_upload_max_bytes: u64,
    /// How long, in seconds, a segment upload may go without a call before it is
    /// dropped and its memory returned. An upload is also dropped when the connection
    /// that began it closes; one that is committing is kept until its commit ends.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    pub segment_upload_idle_secs: u64,
    /// `http://host[:port]` of the S3 service (`--store=s3`).
    #[arg(long, required_if_eq("store", "s3"))]
    pub s3_endpoint: Option<String>,
    /// The bucket (`--store=s3`).
    #[arg(long, required_if_eq("store", "s3"))]
    pub s3_bucket: Option<String>,
    /// The signing region.
    #[arg(long, default_value = "us-east-1")]
    pub s3_region: String,
    /// The key prefix. Under `--meta-log=memory` each start writes under
    /// `<prefix><start time>/`; under `--meta-log=raft` every start writes under
    /// `<prefix><log id>/`, fixed when the log is created (a later start with another
    /// prefix is refused).
    #[arg(long, default_value = "kbf/")]
    pub s3_prefix: String,
    /// The store refuses to overwrite a key (MinIO and RustFS do).
    #[arg(long)]
    pub s3_conditional_put: bool,
    /// How long, in milliseconds, the oldest blob of an open segment waits for
    /// concurrent uploads to join it before the segment is sealed and written
    /// (`docs/design/write-path.md`, section 3.1). No caller is answered before its
    /// segment has committed, so this is added to an upload's latency at most.
    #[arg(
        long,
        default_value_t = kbf_front::WriteConfig::DEFAULT.linger.as_millis() as u64,
        value_parser = clap::value_parser!(u64).range(0..=1_000)
    )]
    pub write_linger: u64,
    /// Blobs of this many bytes or more are written as a segment of their own, past
    /// the group, so they neither wait for the linger nor hold up small writes.
    #[arg(long, default_value_t = kbf_front::WriteConfig::DEFAULT.own_segment_bytes)]
    pub write_own_segment_bytes: u64,
    /// The most bytes uploads may hold at once. A ByteStream Write is admitted for its
    /// blob's size before it reads past its first message, a BatchUpdateBlobs for its
    /// total, a segment upload's Begin for its whole length; a caller over the bound
    /// waits, nothing is refused for load. At least the largest blob (1 GiB), so one
    /// can always be admitted, and at least `--segment-upload-max-bytes`. Not covered
    /// here: each HTTP/2 stream's message as tonic decodes it, before the handler runs,
    /// up to 1 GiB per stream and 200 streams per connection on either listener
    /// (`kbf_front::MAX_CONCURRENT_STREAMS`); the in-flight layer counts those against
    /// one budget of `kbf_front::MESSAGE_BYTES_IN_FLIGHT`.
    #[arg(
        long,
        default_value_t = kbf_front::WriteConfig::DEFAULT.bytes_in_flight,
        value_parser = clap::value_parser!(u64).range(kbf_front::MAX_BLOB_BYTES..)
    )]
    pub write_bytes_in_flight: u64,
    /// The most segment PUTs, each with its footer read-back, in flight at once. A
    /// segment sealed while all are busy waits; new blobs go to a new open segment.
    #[arg(long, default_value_t = kbf_front::WriteConfig::DEFAULT.puts_in_flight)]
    pub write_puts_in_flight: NonZeroUsize,
    /// How many times a segment PUT is tried. Only a PUT that did not complete
    /// (connect, reset, timeout) or that the store answered 5xx is tried again, after
    /// 100 ms and then four times longer each time, under a new object key; if every
    /// attempt fails, the segment's blobs are answered UNAVAILABLE.
    #[arg(
        long,
        default_value_t = kbf_front::WriteConfig::DEFAULT.put_attempts.get(),
        value_parser = clap::value_parser!(u32).range(1..=5)
    )]
    pub store_put_attempts: u32,
    /// The local write tier (`docs/design/write-path.md`, section 6). Off unless
    /// `--write-tier-dir` is given.
    #[command(flatten)]
    pub tier: TierArgs,
}

/// This server in the tier registry. A farm runs one server today
/// (`docs/design/deployment-topology.md`); each gets its own id with HA.
pub const SERVER: ServerId = ServerId::new(1);

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
    /// A segment upload could declare more than uploads may hold at once.
    #[error(
        "--segment-upload-max-bytes {cap} is over --write-bytes-in-flight {bound}: \
         a segment upload holds its ranges in memory under that bound"
    )]
    SegmentUploadOverWriteBound { cap: u64, bound: u64 },
    /// `--meta-log=raft` with `--store=memory`.
    #[error(
        "--meta-log=raft needs --store=s3: a durable index over blobs that die with the process"
    )]
    RaftWithoutS3,
    /// `--raft-dir` without `--meta-log=raft`.
    #[error("--raft-dir is read only with --meta-log=raft")]
    RaftDirUnused,
    /// The REAPI policy file is not a policy.
    #[error("--reapi-auth-policy: {path}: {source}")]
    Policy {
        path: PathBuf,
        #[source]
        source: PolicyError,
    },
}

impl Args {
    /// The write tier's directory and options, or `None` when the tier is off.
    ///
    /// The tier runs over either metadata log. Under `--meta-log=raft` its start raises
    /// the log to machine version 2 once, and recovers the directory against the
    /// committed state; under `--meta-log=memory` it clears the directory at every start
    /// ([`crate::write_tier::attach`]).
    #[must_use]
    pub fn write_tier(&self) -> Option<(PathBuf, TierOptions)> {
        self.tier.config()
    }

    /// The listeners these flags describe.
    ///
    /// # Errors
    /// A TLS file cannot be read; `--segment-upload-max-bytes` is over
    /// `--write-bytes-in-flight`.
    pub fn listeners(&self) -> Result<Listeners, ConfigError> {
        self.segment_upload_fits_write_bound()?;
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
            store_probe_timeout: Duration::from_millis(self.readyz_store_timeout_ms),
            invocation_memory: usize::try_from(self.invocation_memory).unwrap_or(usize::MAX),
            segments: SegmentsConfig {
                max_bytes: self.segment_upload_max_bytes,
                idle: Duration::from_secs(self.segment_upload_idle_secs),
            },
        })
    }

    /// Refuses a `--segment-upload-max-bytes` over `--write-bytes-in-flight`: an upload
    /// of the whole cap would hold its ranges in memory past the bound uploads share.
    /// A function of its own: only the unit test reaches the refusal, and the integration
    /// tests reach the rest of `listeners`, so neither would cover all of one function.
    fn segment_upload_fits_write_bound(&self) -> Result<(), ConfigError> {
        if self.segment_upload_max_bytes > self.write_bytes_in_flight {
            return Err(ConfigError::SegmentUploadOverWriteBound {
                cap: self.segment_upload_max_bytes,
                bound: self.write_bytes_in_flight,
            });
        }
        Ok(())
    }

    /// The listeners that serve plain text on an address that is not loopback, as the
    /// binary's start-line warning names them: `reapi` (no `--reapi-tls-cert`), then
    /// `worker` (no `--worker-tls-cert`). Empty when every plain-text listener is on
    /// loopback. Nothing refuses such a bind; whether plain text is acceptable on that
    /// network is the operator's call.
    #[must_use]
    pub fn plain_text_off_loopback(&self) -> Vec<&'static str> {
        let reapi = self.reapi_tls_cert.is_none() && !loopback_only(self.listen.ip());
        let worker = self.worker_tls_cert.is_none() && !loopback_only(self.worker_listen.ip());
        [("reapi", reapi), ("worker", worker)]
            .into_iter()
            .filter_map(|(name, plain)| plain.then_some(name))
            .collect()
    }

    /// The REAPI listener's policy: the file `--reapi-auth-policy` names, or
    /// [`Policy::allow_all`] without it; with `--invocations-open`, its
    /// `invocations_get` and `blobs_get` are `allow` where the file names neither.
    ///
    /// # Errors
    /// The file cannot be read, or is not a policy ([`Policy::from_json`]).
    pub fn reapi_auth_policy(&self) -> Result<Policy, ConfigError> {
        let mut policy = match &self.reapi_auth_policy {
            None => Policy::allow_all(),
            Some(path) => {
                let text =
                    std::fs::read_to_string(path).map_err(|source| ConfigError::PolicyRead {
                        path: path.clone(),
                        source,
                    })?;
                Policy::from_json(&text).map_err(|source| ConfigError::Policy {
                    path: path.clone(),
                    source,
                })?
            }
        };
        if self.invocations_open {
            let allow = || -> Arc<dyn Authorizer> { Arc::new(AllowAuthorizer) };
            policy.invocations_get.get_or_insert_with(allow);
            policy.blobs_get.get_or_insert_with(allow);
        }
        Ok(policy)
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
        Ok(Some(Api { listen, token }))
    }

    /// How the cache groups and bounds uploads: `--write-linger`,
    /// `--write-own-segment-bytes`, `--write-bytes-in-flight`, `--write-puts-in-flight`
    /// and `--store-put-attempts`.
    #[must_use]
    pub fn write_config(&self) -> WriteConfig {
        WriteConfig {
            linger: Duration::from_millis(self.write_linger),
            own_segment_bytes: self.write_own_segment_bytes,
            bytes_in_flight: self.write_bytes_in_flight,
            puts_in_flight: self.write_puts_in_flight,
            // clap refuses 0.
            put_attempts: NonZeroU32::new(self.store_put_attempts).unwrap_or(NonZeroU32::MIN),
        }
    }

    /// The raft directory under `--meta-log=raft`; `None` under `--meta-log=memory`.
    ///
    /// # Errors
    /// [`ConfigError::RaftWithoutS3`] for `--meta-log=raft --store=memory`, and
    /// [`ConfigError::RaftDirUnused`] for a `--raft-dir` under `--meta-log=memory`.
    pub fn raft_dir(&self) -> Result<Option<&Path>, ConfigError> {
        match (self.meta_log, self.store, self.raft_dir.as_deref()) {
            (MetaLogKind::Memory, _, None) => Ok(None),
            (MetaLogKind::Memory, _, Some(_)) => Err(ConfigError::RaftDirUnused),
            (MetaLogKind::Raft, StoreKind::Memory, _) => Err(ConfigError::RaftWithoutS3),
            // clap requires `--raft-dir` with `--meta-log=raft`.
            (MetaLogKind::Raft, StoreKind::S3, dir) => Ok(dir),
        }
    }

    /// The S3 store and the key prefix of this start, `<prefix><start time>/` (the
    /// one `--meta-log=memory` writes under), for `--store=s3`. `env` reads an
    /// environment variable (the S3 key pair).
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

/// Whether a listener bound to `ip` is reachable only from this host: a loopback
/// address, an IPv4-mapped one (`::ffff:127.0.0.1`) included. A wildcard (`0.0.0.0`,
/// `::`) and every other address can be reached from other machines.
#[must_use]
pub fn loopback_only(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

fn read(path: &PathBuf) -> Result<Vec<u8>, ConfigError> {
    std::fs::read(path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::num::{NonZeroU32, NonZeroUsize};
    use std::path::Path;
    use std::time::Duration;

    use clap::Parser;
    use kbf_front::{MAX_BLOB_BYTES, WriteConfig};

    use super::{Args, loopback_only};

    /// Catches: the write tier refused (or silently off) over either metadata log, its
    /// options lost on the way, or the tier on when `--write-tier-dir` is not given.
    #[test]
    fn the_write_tier_runs_over_either_metadata_log() {
        let parse = |extra: &[&str]| {
            let base = ["kbf-server", "--store", "s3", "--s3-endpoint", "http://e:9"];
            let rest = ["--s3-bucket", "b"];
            Args::parse_from(base.iter().chain(&rest).chain(extra))
        };
        let tier = ["--write-tier-dir", "/t", "--write-tier-bytes", "9"];
        assert_eq!(parse(&[]).write_tier(), None);
        let on = parse(&tier).write_tier();
        assert_eq!(on.map(|(d, o)| (d, o.budget)), Some(("/t".into(), 9)));
        let raft = ["--meta-log", "raft", "--raft-dir", "/r"];
        let both: Vec<&str> = tier.iter().chain(&raft).copied().collect();
        let both = parse(&both);
        assert_eq!(both.raft_dir().expect("raft"), Some(Path::new("/r")));
        let on = both.write_tier();
        assert_eq!(on.map(|(d, o)| (d, o.budget)), Some(("/t".into(), 9)));
    }

    /// Catches a `--segment-upload-*` flag that does not reach the listeners, a default
    /// other than the design's (1 GiB + 1 MiB, 60 s), and a cap below one largest blob
    /// and its footer accepted (a `Begin` for such a blob would be refused for good).
    #[test]
    fn segment_upload_flags_reach_the_listeners() {
        let defaults = Args::parse_from(["kbf-server"])
            .listeners()
            .expect("listeners");
        assert_eq!(defaults.segments, kbf_front::SegmentsConfig::DEFAULT);
        assert_eq!(defaults.segments.max_bytes, (1 << 30) + (1 << 20));
        assert_eq!(defaults.segments.idle, std::time::Duration::from_secs(60));
        let set = Args::parse_from([
            "kbf-server",
            "--segment-upload-max-bytes=2147483648",
            "--segment-upload-idle-secs=5",
        ])
        .listeners()
        .expect("listeners");
        assert_eq!(set.segments.max_bytes, 1 << 31);
        assert_eq!(set.segments.idle, std::time::Duration::from_secs(5));
        let below = format!(
            "--segment-upload-max-bytes={}",
            (1u64 << 30) + (1 << 20) - 1
        );
        for refused in [below.as_str(), "--segment-upload-idle-secs=0"] {
            assert!(
                Args::try_parse_from(["kbf-server", refused]).is_err(),
                "{refused} accepted"
            );
        }
    }

    /// Catches a server that starts with `--segment-upload-max-bytes` over
    /// `--write-bytes-in-flight` (mutant: the check removed). A segment upload holds its
    /// ranges in memory under the write bound, so an upload of the whole cap would hold
    /// more than the bound admits. A cap equal to the bound starts; one byte of bound
    /// less is refused, naming both flags.
    #[test]
    fn a_segment_upload_cap_over_the_write_bound_is_refused() {
        let cap = kbf_front::MIN_SEGMENT_UPLOAD_BYTES;
        let args = |bound: u64| {
            Args::parse_from([
                "kbf-server".to_owned(),
                format!("--segment-upload-max-bytes={cap}"),
                format!("--write-bytes-in-flight={bound}"),
            ])
        };
        args(cap).listeners().expect("a cap equal to the bound");
        let refused = args(cap - 1)
            .listeners()
            .map(drop)
            .expect_err("a cap over the bound");
        let message = refused.to_string();
        assert!(message.contains("--segment-upload-max-bytes"), "{message}");
        assert!(message.contains("--write-bytes-in-flight"), "{message}");
    }

    /// Catches a server that starts with a `--segment-upload-max-bytes` larger than one
    /// admission counts (mutant: the flag's upper limit removed). `Cache::admit` takes at
    /// most `kbf_front::MAX_ADMISSION_BYTES` (4 GiB - 1) for one call, so an 8 GiB
    /// `Begin` under a 16 GiB bound would hold 8 GiB of ranges while counting 4 GiB, and
    /// other uploads could push what is held past the bound. A cap of exactly the most
    /// one admission counts starts; one byte more is refused, however large the bound.
    #[test]
    fn a_segment_upload_cap_over_one_admission_is_refused() {
        let bound = format!("--write-bytes-in-flight={}", 16u64 << 30);
        let cap = |cap: u64| format!("--segment-upload-max-bytes={cap}");
        let most = kbf_front::MAX_ADMISSION_BYTES;
        Args::try_parse_from(["kbf-server", &cap(most), &bound])
            .expect("a cap one admission counts in full")
            .listeners()
            .expect("under the bound");
        for over in [most + 1, 8u64 << 30] {
            let refused = Args::try_parse_from(["kbf-server", &cap(over), &bound])
                .map(drop)
                .expect_err("a cap over one admission");
            let message = refused.to_string();
            assert!(message.contains("--segment-upload-max-bytes"), "{message}");
        }
    }

    /// Catches: a `--write-bytes-in-flight` below the largest blob accepted (a Write of
    /// that blob would wait for admission forever), a flag wired to the wrong field of
    /// the write configuration, and defaults other than the design's.
    #[test]
    fn write_flags_make_the_write_config() {
        let defaults = Args::parse_from(["kbf-server"]).write_config();
        assert_eq!(defaults, WriteConfig::DEFAULT);
        let set = Args::parse_from([
            "kbf-server",
            "--write-linger",
            "7",
            "--write-own-segment-bytes",
            "12345",
            "--write-bytes-in-flight",
            &MAX_BLOB_BYTES.to_string(),
            "--write-puts-in-flight",
            "9",
            "--store-put-attempts",
            "5",
        ]);
        let want = WriteConfig {
            linger: Duration::from_millis(7),
            own_segment_bytes: 12345,
            bytes_in_flight: MAX_BLOB_BYTES,
            puts_in_flight: NonZeroUsize::new(9).expect("9"),
            put_attempts: NonZeroU32::new(5).expect("5"),
        };
        assert_eq!(set.write_config(), want);
        let below = (MAX_BLOB_BYTES - 1).to_string();
        for (flag, value) in [
            ("--write-bytes-in-flight", below.as_str()),
            ("--write-puts-in-flight", "0"),
            ("--store-put-attempts", "0"),
            ("--store-put-attempts", "6"),
            ("--write-linger", "1001"),
        ] {
            let refused = Args::try_parse_from(["kbf-server", flag, value]);
            assert!(refused.is_err(), "{flag} {value} was accepted");
        }
    }

    /// Catches: a `loopback_only` that knows only wildcards (`is_unspecified`), so one
    /// interface other machines reach (`192.0.2.10`, a documentation address standing
    /// in for a LAN or mesh one) counts as loopback; and one that drops `to_canonical`,
    /// so the IPv4-mapped loopback address does not.
    #[test]
    fn only_loopback_addresses_are_loopback_only() {
        for (ip, want) in [
            ("127.0.0.1", true),
            ("::1", true),
            ("::ffff:127.0.0.1", true),
            ("0.0.0.0", false),
            ("::", false),
            ("192.0.2.10", false),
        ] {
            assert_eq!(loopback_only(ip.parse().expect("an address")), want, "{ip}");
        }
    }

    /// Catches: a call site in [`Args::plain_text_off_loopback`] that does not go
    /// through `loopback_only`. One listener is on a single non-loopback interface
    /// (`192.0.2.10`, which a wildcard-only check such as `is_unspecified` misses) and
    /// the other on the IPv4-mapped loopback address (`::ffff:127.0.0.1`, which
    /// `is_loopback` without `to_canonical` misses), each way round, so either
    /// mistake at either listener changes the list.
    #[test]
    fn plain_text_off_loopback_checks_each_listener_through_loopback_only() {
        for (listen, worker_listen, want) in [
            ("192.0.2.10:0", "[::ffff:127.0.0.1]:0", vec!["reapi"]),
            ("[::ffff:127.0.0.1]:0", "192.0.2.10:0", vec!["worker"]),
        ] {
            let args = Args::parse_from([
                "kbf-server",
                "--listen",
                listen,
                "--worker-listen",
                worker_listen,
            ]);
            assert_eq!(
                args.plain_text_off_loopback(),
                want,
                "--listen {listen} --worker-listen {worker_listen}"
            );
        }
    }
}
