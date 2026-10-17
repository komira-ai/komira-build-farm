//! `kbf-server`: the farm server, single node. It wires the front, the scheduler, the
//! metadata core and storage together; the decisions are theirs.
//!
//! - REAPI on one listener: the cache services and `Execution` from `kbf-front`, over
//!   a [`kbf_front::Cache`] (metadata in a single-voter Raft log on the local disk,
//!   [`raft_log`]; an S3 or local-directory object store, with an optional local hot
//!   store in front of it; the flags that choose them are `--cold-store`,
//!   `--hot-store` and `--meta-dir`, see [`stores`]).
//! - `kbf.worker.v1` on another ([`worker`]): daemons register, heartbeat, receive
//!   lease offers and `Start`s, and report results. Under mutual TLS a daemon's
//!   certificate must name its node, and a deny list refuses certificates and nodes
//!   ([`identity`]). The same listener serves daemons `ByteStream` for their blob
//!   reads and writes ([`blobs`]), each call admitted by the same rules.
//! - The operator API ([`api`]): HTTP/JSON under `/v1` on a third listener, off unless
//!   `--api-listen` is given; `GET /v1/nodes`, and cordon, drain and uncordon, which
//!   need the token of `--api-token-file` ([`token`]). On the same listener,
//!   `GET /healthz` and `GET /readyz` for a front's health check ([`health`]), and
//!   `GET /metrics`, the cache's upload-path metrics for Prometheus ([`metrics`]).
//!   The REAPI listener answers `/readyz`'s readiness over `grpc.health.v1`
//!   ([`grpc_health`]). And the build pages, `GET /v1/invocations/{id}` and below,
//!   over the invocation records kept in memory ([`http_invocations`]), off unless
//!   the REAPI policy names `invocations.getAuthorizer` (or `--invocations-open` is
//!   given); and `GET /v1/blobs/{digest}`, a CAS blob's bytes for a browser
//!   ([`http_blobs`]), off unless the policy also names `blobs.getAuthorizer`.
//! - A drain of the frontend ([`drain`]) that a stop signal or `POST /v1/frontend/drain`
//!   begins, a second signal escalates, and `kbf cluster` calls through [`api_client`].
//! - Rollouts ([`rollout`]): the record's store (in memory for now) and the driver that
//!   cordons, drains and hands drained nodes their update, as far as `updating`.
//! - Node rolling ([`update`]): the pure picker and node step of a split-mode update
//!   (`docs/design/split-mode-updates.md`, sections 2.6 to 2.11), which host goes next and
//!   its one next move. No controller drives them yet.
//! - The MDM gate ([`mdm`]): the server's verbs at `kbf-mdm-gate` (inventory, enforce
//!   and withdraw a macOS build, install an allowlisted profile; no erase), a client
//!   over mutual TLS, and the polling of an update's DDM progress. Not yet called by
//!   the rollout driver.
//! - Build history ([`history`]): off unless `--history-objects` is given, each attempt and
//!   cache hit of an invocation a client named is recorded in an object store, off the
//!   path of the call it belongs to, with a run id the build pages name the run by. It
//!   drops and counts when a store is slow; it never delays or fails a call.
//! - The durable log ([`log_file`]): a size-capped rotating file in `--log-dir` (by default
//!   `log/` in the hot store's directory) that gets every event stderr gets.
//! - The [`farm::Farm`] core: `kbf-sched` decides, the farm carries out its effects.
//!   `Start` only after the grant commits; a result is accepted only from the node
//!   holding the operation's current lease, and only an accepted result is written to
//!   the action cache, before the callers are answered. Clients never write it.
//!
//! - REAPI authentication and authorization (`kbf-auth`): the REAPI listener runs the
//!   policy of `--reapi-auth-policy` (`docs/reapi-auth.md`), an authentication layer
//!   on that listener alone and an authorizer per kind of call in the services;
//!   without the flag every call is accepted and allowed.
//!
//! - The storage node ([`storage_node`], `kbf storage`): the same cache and stores,
//!   served alone over gRPC by `kbf_storage::service` (blobs through the standard
//!   protocols, the action cache through `kbf.storage.v1`), each call admitted by the
//!   role in the caller's certificate. No scheduler, REAPI or state store.
//!   [`storage_half`] is the same start for `kbf storage-and-daemon`, keeping the storage
//!   its listener serves for the daemon half in the same process.
//!
//! - The frontend ([`frontend`], `kbf frontend`): the REAPI listener, the scheduler,
//!   the worker streams and the operator API, with no cache of its own. CAS, ByteStream
//!   and the action cache are proxied to a storage node over mutual TLS, Execute and the
//!   scheduler read and write that node, and `/readyz` follows it and the state store.
//!
//! Not yet: the scheduler's state in the Raft log (the control log is in-process: a
//! record commits as soon as it is appended, see [`farm`]), REAPI authentication other than the policies
//! `kbf-auth` builds (`allow`, `deny`, `any`, `all`), operator roles (the
//! API's writes need one token, [`token`]), the `x-kbf-qos` header, learned sizes,
//! capability matching, and the daemon-side `ResultAck` handling (issue #26).

pub mod action_usage;
pub mod api;
pub mod api_client;
pub mod blobs;
#[cfg(test)]
mod build_commit;
pub mod config;
pub mod drain;
pub mod farm;
mod fault;
mod flag_set;
pub mod fleet;
pub mod front_maps;
pub mod frontend;
pub mod grpc_health;
pub mod handover;
pub mod health;
pub mod history;
pub mod http_blobs;
pub mod http_invocations;
pub mod identity;
pub mod join;
mod listen;
mod local;
pub mod log_file;
pub mod machine;
mod map_push;
pub mod mdm;
pub mod members;
pub mod members_rpc;
mod memory;
pub mod metrics;
mod missing;
pub mod node_map;
pub mod raft_log;
pub mod replication;
pub mod rollout;
mod run_record;
pub mod serve;
pub mod stamp;
mod start;
mod startup_pass;
pub mod state_store;
pub mod storage_half;
pub mod storage_node;
pub mod stores;
pub mod streams;
mod timeout;
pub mod token;
pub mod update;
pub mod worker;
pub mod worker_roles;
pub mod write_tier;

pub use config::{Args, ConfigError, Role, SERVER, s3_store};
pub use drain::{Drain, DrainStatus};
pub use farm::{DONE_FLOOR, EpochPolicy, Farm, SESSION_RENEWAL, StateError};
pub use health::Readiness;
pub use identity::{DenyList, DenyListError, Peers};
pub use kbf_sched::MemoryLadder;
pub use kbf_tls::{TlsError, TlsFiles, TlsSource};
pub use local::LocalStore;
pub use node_map::NodeMap;
pub use raft_log::{RaftLogError, RaftMetaLog};
pub use serve::{
    Api, Bound, BoundStorage, Listeners, ServeError, StorageListener, StreamTiming, WorkerTls,
    bind_server, bind_server_with_api, bind_server_with_map, bind_server_with_options,
    bind_server_with_policy, bind_server_with_state, bind_server_with_storage, bind_service,
    bind_storage,
};
pub use start::{ServerHandle, interrupted, serve_with, sizes_on, start, start_interruptible};
pub use state_store::{StateStore, StateStoreUri, StateStoreUriError};
pub use streams::{StreamArgs, StreamLimits};
pub use worker::WorkerService;

/// The commit the server was built from: 12 hex digits, or `unknown` when the build
/// had no git checkout. `build.rs` embeds it (`src/build_commit.rs` has the rule,
/// including the CI-only `KBF_BUILD_COMMIT_OVERRIDE`).
pub const BUILD_COMMIT: &str = env!("KBF_BUILD_COMMIT");

/// The server's version as it reports it in `--version`, the start line and the
/// `server` field of `GET /v1/nodes`: the package version, `+`, and [`BUILD_COMMIT`].
pub const SERVER_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("KBF_BUILD_COMMIT"));

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::{BUILD_COMMIT, SERVER_VERSION};

    /// `git rev-parse --short=12 HEAD` in this crate's checkout.
    fn git_head() -> String {
        let head = Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .expect("git runs");
        assert!(head.status.success(), "the tests run in a git checkout");
        String::from_utf8(head.stdout)
            .expect("UTF-8")
            .trim()
            .to_owned()
    }

    /// Catches: a version that names the package version alone, so two builds of it
    /// read the same in `--version`, the start line and `/v1/nodes`; and a commit that
    /// is not the one built (the build script fell back to `unknown`, embedded another,
    /// or ignored `KBF_BUILD_COMMIT_OVERRIDE` when the build set it).
    #[test]
    fn the_version_names_the_commit_built() {
        let (package, commit) = SERVER_VERSION.split_once('+').expect("a + in the version");
        assert_eq!(package, env!("CARGO_PKG_VERSION"));
        assert_eq!(commit, BUILD_COMMIT);
        let stamp = option_env!("KBF_BUILD_COMMIT_OVERRIDE");
        let expected = stamp.map_or_else(git_head, str::to_owned);
        assert_eq!(commit, expected);
        assert_eq!(commit.len(), 12, "{commit}");
        assert!(commit.bytes().all(|b| b.is_ascii_hexdigit()), "{commit}");
    }
}
