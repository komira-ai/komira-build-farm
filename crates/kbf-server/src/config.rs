//! Command-line flags. Every setting is a flag; the only environment variables read are
//! the standard S3 key pair, `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (a secret
//! never goes on the command line). Where the server keeps its bytes is two store URIs
//! and, sometimes, one directory ([`crate::stores`]): `--cold-store`, `--hot-store`,
//! `--meta-dir`.

use std::net::{IpAddr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use kbf_front::WriteConfig;
use kbf_objstore::Capabilities;
use kbf_objstore::s3::{Credentials, S3Config, S3ConfigError, S3Store};
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

use kbf_auth::{AllowAuthorizer, Authorizer, Policy, PolicyError};
use kbf_front::SegmentsConfig;
use kbf_meta::ServerId;
use kbf_threads::disk::FreeSpace;

use crate::identity::{DenyList, DenyListError};
use crate::serve::{Api, Listeners, StreamTiming, WorkerTls};
use crate::stores::{S3Uri, StoreFlags, StoreUri, Stores, StoresError};
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
    /// The node's one cache, when a daemon in this process shares the hot store's disk
    /// (`kbf-all-in-one`, `kbf_storage::local_cache`). No flag sets it.
    #[arg(skip)]
    pub local_cache: Option<Arc<kbf_storage::local_cache::LocalCache>>,
    /// The durable object store, where every blob ends up: `file:///absolute/path` (a
    /// directory, `kbf_objstore::FsStore`: locked while the server runs, a second
    /// server on it is refused; a write that would leave its filesystem with less free
    /// than it keeps, 5% of its size and at least 1 GiB, is refused and wakes the
    /// evictor, which forgets the least recently used objects (never one used in the
    /// last minute) and deletes their files while less than twice that is free) or
    /// `s3://bucket/prefix?endpoint=http://host:port` (MinIO, RustFS; the key pair is
    /// `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`; query keys `region`,
    /// `conditional-put=true` for a store that refuses to overwrite a key, and
    /// `min-put-rate=BYTES_PER_SEC`, the slowest rate a PUT body may reach the store at:
    /// a PUT is cut after 60 seconds plus its body's length at that rate, 1 MiB/s by
    /// default). Objects are keyed under `<prefix><log id>/` (`kbf/` for a directory and
    /// for an S3 URI without a path), the id of the metadata log. A directory is
    /// checked against the index by a start-up pass in the background; a lost or
    /// damaged object is also found when it is read, or before an action-cache hit
    /// that needs it is served (a miss). There is no `memory:` store:
    /// a `file://` path on a tmpfs is one.
    #[arg(long, value_name = "URI")]
    pub cold_store: Option<StoreUri>,
    /// The fast tier in front of the cold store: `file:///absolute/path`, a directory
    /// on an SSD (or a tmpfs). Uploads land there and are flushed to the cold store in
    /// the background; reads are served from it while a blob is still there. Its
    /// budget is not a flag: 80% of the filesystem's free space less what it keeps free
    /// (5% of its size, at least 1 GiB, and never more than half of what is free), at
    /// most `--hot-store-max-bytes`; at the budget, writes
    /// take the direct path to the cold store. Without it, uploads go straight to the
    /// cold store. A daemon's outputs (a segment upload, or the in-process plane) and a
    /// ByteStream Write over 16 MiB are written through to it as they arrive, so a
    /// large one is never held in memory.
    /// Without it, or when it has no room, a daemon's blob over 16 MiB
    /// (`kbf_front::MAX_UNSTAGED_BLOB_BYTES`) is streamed to the cold store as it
    /// arrives (a ByteStream Write, or the in-process plane's own stream), never held
    /// whole and never refused for its size. The first start with a hot store raises
    /// the metadata log to machine version 2, once; a binary that runs only version 1 no longer reads it.
    /// The metadata log lives in `<dir>/meta`, unless `--meta-dir` says otherwise.
    #[arg(long, value_name = "URI")]
    pub hot_store: Option<StoreUri>,
    /// The most bytes the hot store's block files may hold before they are flushed,
    /// lower than its automatic budget.
    #[arg(long, value_name = "BYTES", requires = "hot_store", value_parser = clap::value_parser!(u64).range(1..))]
    pub hot_store_max_bytes: Option<u64>,
    /// Abandons this server's hot store tier `<id>` at start: every object staged on
    /// it is marked unreachable as missing (its blobs answer `Unavailable` until
    /// uploaded again; nothing is answered `NOT_FOUND`), and the directory becomes a new
    /// tier. For a lost or replaced disk, after a start was refused because the
    /// directory's `TIER` file is missing or names another tier. The id is the one the
    /// refusal names.
    #[arg(long, value_name = "ID", requires = "hot_store", value_parser = clap::value_parser!(u64).range(1..))]
    pub hot_store_abandon: Option<u64>,
    /// The directory of the metadata log (the CAS index and the action cache, a
    /// single-voter Raft log), on a local disk; created if missing. An empty directory
    /// gets a new log with a new id; every later start over it reads the id from the
    /// directory. Left out, it is `<hot store directory>/meta`; required when there is
    /// no `file://` hot store. Given, it wins over the hot store's directory.
    #[arg(long, value_name = "DIR")]
    pub meta_dir: Option<PathBuf>,
    /// For tests only: the free bytes a `file://` cold store's filesystem must keep, in
    /// place of 5% of it, at least 1 GiB (a test's disk would decide what is evicted).
    #[arg(long, hide = true, value_name = "BYTES", requires = "cold_store")]
    pub cold_store_floor_bytes: Option<u64>,
    /// For tests of eviction only: a `file://` cold store's filesystem is taken to be
    /// this many bytes, holding nothing but the store's object files.
    #[arg(long, hide = true, value_name = "BYTES", requires = "cold_store")]
    pub cold_store_size_bytes: Option<u64>,
    /// For tests of eviction only: how long after its last use an object in a
    /// `file://` cold store is never evicted, in milliseconds.
    #[arg(long, hide = true, default_value_t = kbf_front::EVICT_GRACE.as_millis() as u64)]
    pub cold_store_evict_grace_ms: u64,
    /// The directory of the durable log file, `kbf-server.log` and its rotated
    /// `kbf-server.log.1`, `.2`, ... (created if missing): every event stderr gets, in
    /// the same format, kept across restarts. Left out, it is `<hot store directory>/log`
    /// when the hot store is a directory; with neither, there is no file and the server
    /// logs to stderr alone (the start logs which). A directory or file that cannot be
    /// written never stops the server ([`crate::log_file`]).
    #[arg(long, value_name = "DIR")]
    pub log_dir: Option<PathBuf>,
    /// The most bytes one log file holds before it is rotated; a line longer than this
    /// goes alone to a fresh file. With `--log-files` it bounds the log's size on disk.
    #[arg(
        long,
        value_name = "BYTES",
        default_value_t = crate::log_file::DEFAULT_MAX_BYTES,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    pub log_max_bytes: u64,
    /// How many log files are kept, the current one included; rotation drops the
    /// oldest. The log never exceeds `--log-max-bytes` times this, plus one line.
    #[arg(
        long,
        value_name = "N",
        default_value_t = crate::log_file::DEFAULT_FILES,
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    pub log_files: u32,
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
    /// The memory rungs of an action, in GiB, ascending and each at least 1. The first
    /// is what an action that names no size (`kbf-book-mem-gib`) books. An action that
    /// passes its own memory limit runs again on the next rung above what it booked,
    /// and after the last on a whole host: the memory of the largest node that could
    /// run it, so that no other lease fits there. A rung not below that node's memory
    /// is skipped for the whole host. An action that does finish keeps its rung: later
    /// runs of it start there.
    #[arg(
        long,
        value_delimiter = ',',
        default_values_t = kbf_sched::MemoryLadder::DEFAULT_GIB
    )]
    pub memory_ladder_gib: Vec<u64>,
    /// How long, in seconds, a finished operation is kept after its callers are
    /// answered. Within it, WaitExecution on the operation's name streams its result
    /// (a client whose Execute stream broke gets a result the action cache does not
    /// keep, such as a failure); after it, the name is NOT_FOUND. Every operation
    /// finished within it is held in memory.
    #[arg(long, default_value_t = kbf_sched::FINISHED_RETENTION.as_secs())]
    pub finished_retention_secs: u64,
    /// How long, in seconds, a stop (SIGINT or SIGTERM) waits for REAPI clients to
    /// finish: it refuses new Execute calls (UNAVAILABLE, which Buck2 retries) and
    /// sends GOAWAY, and every open Execute and WaitExecution stream stays open until
    /// its action is done. Buck2 never recovers a stream that breaks mid-action, so the
    /// default, 0, is no deadline; set one at least as long as the longest action. When
    /// it runs out, the open streams end UNAVAILABLE (a client still uploading is cut
    /// off).
    #[arg(long, default_value_t = 0)]
    pub shutdown_timeout_secs: u64,
    /// How often, in seconds, every open Execute and WaitExecution stream is sent its
    /// operation again (same name, not done, current stage), so a load balancer's idle
    /// timeout never cuts a long action. 0 sends none.
    #[arg(long, default_value_t = 30)]
    pub progress_interval_secs: u64,
    /// How often, in seconds, the REAPI and worker listeners send an HTTP/2 PING on a
    /// quiet connection. 0 sends none (and `--http2-keepalive-timeout-secs` is unused).
    #[arg(long, default_value_t = 30)]
    pub http2_keepalive_interval_secs: u64,
    /// How long, in seconds, a PING may go unanswered before the connection is closed.
    #[arg(long, default_value_t = 20)]
    pub http2_keepalive_timeout_secs: u64,
    /// Thread sizes: the overrides `--async-threads`, `--blocking-threads` and
    /// `--cpu-threads` (see `kbf_threads`). Left out, the server uses every CPU it may
    /// (the async workers and the CPU pool each one thread per logical CPU); it has no
    /// `--reserve-cpus`, which is for the daemon. `--cpu-threads` is the most CPU-bound
    /// jobs the server runs at once on the blocking pool, off the request workers: today the SHA-256 and CRC-32C of
    /// records, in the write tier's flusher before each PUT and, for an upload without
    /// a tier, as its ranges arrive.
    #[command(flatten)]
    pub threads: kbf_threads::PoolArgs,
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
    /// listener may hold in memory (`docs/design/write-path.md`, section 4.2): the
    /// segments it declares that are not written to the hot store as they arrive (all
    /// of them, without one). Not a limit on a blob's size: a segment held in memory
    /// holds no record over 16 MiB (`kbf_front::MAX_UNSTAGED_BLOB_BYTES`), and an upload
    /// with a larger one and no room on the hot store is answered FAILED_PRECONDITION,
    /// so the daemon sends its records by ByteStream, which streams a large blob to the
    /// cold store. Those ranges are held until the upload commits, admitted
    /// under `--write-bytes-in-flight` with every other upload: a `Begin` waits while
    /// that is spent. At least 33 MiB (`kbf_front::MIN_SEGMENT_UPLOAD_BYTES`), a
    /// daemon's upload of small files with room to spare, and at most
    /// `--write-bytes-in-flight`, so any legal `Begin` can be admitted; the server
    /// refuses to start otherwise. At most `kbf_front::MAX_ADMISSION_BYTES`, the most
    /// one admission counts, so a `Begin` never holds more than it is counted for under
    /// the bound.
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
    /// The most bytes uploads may hold at once. A ByteStream Write is admitted before
    /// it reads past its first message, for its blob's size up to 16 MiB
    /// (`kbf_front::MAX_UNSTAGED_BLOB_BYTES`) and for 16 MiB above that (a larger blob
    /// is streamed onto the hot store, or to the cold store without room there, not
    /// held); a BatchUpdateBlobs for its total; a
    /// segment upload's Begin for what it holds in memory. A caller over the bound
    /// waits, nothing is refused for load. At least `--segment-upload-max-bytes` (by
    /// default 33 MiB, under the default 2 GiB). Not covered: each HTTP/2
    /// stream's message as tonic decodes it, before the handler runs, up to 17 MiB per
    /// stream (about 3.4 GiB for 200 streams), times the streams one connection may open
    /// (each listener's `-max-streams` flag).
    #[arg(
        long,
        default_value_t = kbf_front::WriteConfig::DEFAULT.bytes_in_flight,
        value_parser = clap::value_parser!(u64).range(kbf_front::MIN_SEGMENT_UPLOAD_BYTES..)
    )]
    pub write_bytes_in_flight: u64,
    /// The most HTTP/2 streams one connection may have open, per listener.
    #[command(flatten)]
    pub streams: crate::streams::StreamArgs,
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
    /// The most decoded Directories an Execute's input check keeps in memory
    /// (`docs/design/storage.md#input-tree-check`). Least recently used go first.
    #[arg(long, default_value_t = kbf_front::InputTreeConfig::DEFAULT.max_entries)]
    pub input_tree_cache_entries: NonZeroUsize,
    /// The most bytes of decoded Directories the input check keeps (each costs its
    /// digests, 40 bytes apiece, and 160 bytes).
    #[arg(long, default_value_t = kbf_front::InputTreeConfig::DEFAULT.max_bytes)]
    pub input_tree_cache_bytes: u64,
    /// The most Directory blobs the input check reads from the store at once, across
    /// all Executes.
    #[arg(long, default_value_t = kbf_front::InputTreeConfig::DEFAULT.reads_in_flight)]
    pub input_tree_reads_in_flight: NonZeroUsize,
    /// Where the scheduler keeps its state, written before each change is acknowledged and
    /// replayed at start: `sqlite:///absolute/path` (a file on a local disk that this
    /// process owns; every start is a new epoch, so daemons kill the runs of the leases
    /// they held before) or `postgres://...` (a database that outlives the process; a
    /// start keeps its epoch, so daemons keep their runs). Any other scheme is refused.
    /// This build has neither backend yet: a start with the flag is refused. Left out, the
    /// scheduler's state is in memory and a restart loses it
    // Listed beside `--meta-dir`, whose place in the help it takes without moving the
    // flags declared after it.
    #[arg(long, value_name = "URI", display_order = 5)]
    pub state_store: Option<crate::state_store::StateStoreUri>,
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
    #[error("{0} must be set for an s3:// store")]
    MissingEnv(&'static str),
    /// The S3 configuration is refused.
    #[error(transparent)]
    S3(#[from] S3ConfigError),
    /// The store flags do not make a server.
    #[error(transparent)]
    Stores(#[from] StoresError),
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
    /// `kbf storage` would serve plain text off loopback, admitting every caller.
    #[error(
        "--storage-listen {addr} is not a loopback address and the node has no TLS, so it \
         would admit every caller (Write and ActionCommit included); add \
         --storage-tls-cert, --storage-tls-key and --storage-client-ca, or listen on a \
         loopback address"
    )]
    PlaintextStorageOffLoopback { addr: SocketAddr },
    /// `--memory-ladder-gib` is empty, holds a 0, does not ascend, or overflows.
    #[error("--memory-ladder-gib: {0}")]
    MemoryLadder(#[from] kbf_sched::LadderError),
    /// The REAPI policy file is not a policy.
    #[error("--reapi-auth-policy: {path}: {source}")]
    Policy {
        path: PathBuf,
        #[source]
        source: PolicyError,
    },
}

impl Args {
    /// The directory of the durable log file: `--log-dir`, else `log` in the hot
    /// store's directory (beside `meta`), else none.
    #[must_use]
    pub fn log_dir(&self) -> Option<PathBuf> {
        match (&self.log_dir, &self.hot_store) {
            (Some(dir), _) => Some(dir.clone()),
            (None, Some(StoreUri::File(hot))) => Some(hot.join(crate::log_file::HOT_STORE_SUBDIR)),
            (None, _) => None,
        }
    }

    /// Where the flags put the server's bytes and metadata, sized from `space`.
    ///
    /// # Errors
    /// [`StoresError`]: no `--cold-store`, a hot store that is not a directory, no
    /// directory for the metadata log, or a hot store filesystem with no room.
    pub fn stores(&self, space: &dyn FreeSpace) -> Result<Stores, StoresError> {
        StoreFlags {
            cold: self.cold_store.as_ref(),
            hot: self.hot_store.as_ref(),
            hot_max_bytes: self.hot_store_max_bytes,
            hot_abandon: self.hot_store_abandon,
            meta_dir: self.meta_dir.as_deref(),
        }
        .resolve(space)
    }

    /// The listeners these flags describe.
    ///
    /// # Errors
    /// A TLS file cannot be read; `--segment-upload-max-bytes` is over
    /// `--write-bytes-in-flight`; `--memory-ladder-gib` is not a ladder.
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
            memory_ladder: kbf_sched::MemoryLadder::from_gib(&self.memory_ladder_gib)?,
            finished_retention: Duration::from_secs(self.finished_retention_secs),
            shutdown_timeout: match self.shutdown_timeout_secs {
                0 => Duration::MAX,
                secs => Duration::from_secs(secs),
            },
            timing: StreamTiming {
                progress_interval: Duration::from_secs(self.progress_interval_secs),
                keepalive_interval: Duration::from_secs(self.http2_keepalive_interval_secs),
                keepalive_timeout: Duration::from_secs(self.http2_keepalive_timeout_secs),
            },
            store_probe_timeout: Duration::from_millis(self.readyz_store_timeout_ms),
            invocation_memory: usize::try_from(self.invocation_memory).unwrap_or(usize::MAX),
            segments: SegmentsConfig {
                max_bytes: self.segment_upload_max_bytes,
                idle: Duration::from_secs(self.segment_upload_idle_secs),
            },
            streams: self.streams.limits(),
        })
    }

    /// Refuses a `--segment-upload-max-bytes` over `--write-bytes-in-flight`: an upload
    /// of the whole cap would hold its ranges in memory past the bound uploads share.
    /// A function of its own: only the unit test reaches the refusal, and the integration
    /// tests reach the rest of `listeners`, so neither would cover all of one function.
    pub(crate) fn segment_upload_fits_write_bound(&self) -> Result<(), ConfigError> {
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
        let local_cache = self.local_cache.clone();
        Ok(Some(Api {
            listen,
            token,
            local_cache,
        }))
    }

    /// How the cache groups and bounds uploads: `--write-linger`,
    /// `--write-own-segment-bytes`, `--write-bytes-in-flight`, `--write-puts-in-flight`
    /// `--store-put-attempts` and `--cpu-threads`.
    #[must_use]
    pub fn write_config(&self) -> WriteConfig {
        WriteConfig {
            linger: Duration::from_millis(self.write_linger),
            own_segment_bytes: self.write_own_segment_bytes,
            bytes_in_flight: self.write_bytes_in_flight,
            puts_in_flight: self.write_puts_in_flight,
            // clap refuses 0.
            put_attempts: NonZeroU32::new(self.store_put_attempts).unwrap_or(NonZeroU32::MIN),
            cpu_threads: self.cpu_threads(),
        }
    }

    /// How the cache remembers input trees: `--input-tree-cache-entries`,
    /// `--input-tree-cache-bytes` and `--input-tree-reads-in-flight`.
    #[must_use]
    pub const fn input_tree_config(&self) -> kbf_front::InputTreeConfig {
        kbf_front::InputTreeConfig {
            max_entries: self.input_tree_cache_entries,
            max_bytes: self.input_tree_cache_bytes,
            reads_in_flight: self.input_tree_reads_in_flight,
        }
    }

    /// The width of the CPU-bound pool on this machine: `--cpu-threads`, else one
    /// thread per CPU the process may use (`kbf_threads`).
    #[must_use]
    pub fn cpu_threads(&self) -> NonZeroUsize {
        self.cpu_threads_on(kbf_threads::Topology::detect())
    }

    /// [`Args::cpu_threads`] on a given machine shape.
    #[must_use]
    pub fn cpu_threads_on(&self, topology: kbf_threads::Topology) -> NonZeroUsize {
        NonZeroUsize::new(self.threads.resolve(topology).cpu_threads()).unwrap_or(NonZeroUsize::MIN)
    }
}

/// The S3 store of `uri`, its key pair read through `env` (the environment's
/// `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`).
///
/// # Errors
/// The key pair is missing, or the configuration is refused.
pub fn s3_store(uri: &S3Uri, env: impl Fn(&str) -> Option<String>) -> Result<S3Store, ConfigError> {
    let need = |name: &'static str| env(name).ok_or(ConfigError::MissingEnv(name));
    let credentials = Credentials::new(need("AWS_ACCESS_KEY_ID")?, need("AWS_SECRET_ACCESS_KEY")?);
    Ok(S3Store::new(s3_config(uri, credentials))?)
}

/// The store configuration of `uri`.
fn s3_config(uri: &S3Uri, credentials: Credentials) -> S3Config {
    S3Config {
        endpoint: uri.endpoint.clone(),
        region: uri.region.clone(),
        bucket: uri.bucket.clone(),
        credentials,
        capabilities: Capabilities {
            conditional_put: uri.conditional_put,
            object_lock: false,
        },
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(60),
        min_body_rate: Some(uri.min_put_rate),
    }
}

/// Whether a listener bound to `ip` is reachable only from this host: a loopback
/// address, an IPv4-mapped one (`::ffff:127.0.0.1`) included. A wildcard (`0.0.0.0`,
/// `::`) and every other address can be reached from other machines.
#[must_use]
pub fn loopback_only(ip: IpAddr) -> bool {
    ip.to_canonical().is_loopback()
}

pub(crate) fn read(path: &PathBuf) -> Result<Vec<u8>, ConfigError> {
    std::fs::read(path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })
}

#[cfg(test)]
mod tests;
