//! `kbf-server`: the farm server, single node. It wires the front, the scheduler, the
//! metadata core and storage together; the decisions are theirs.
//!
//! - REAPI on one listener: the cache services and `Execution` from `kbf-front`, over
//!   a [`kbf_front::Cache`] (in-memory metadata, and an in-memory or S3 object store).
//! - `kbf.worker.v1` on another ([`worker`]): daemons register, heartbeat, receive
//!   lease offers and `Start`s, and report results. Under mutual TLS a daemon's
//!   certificate must name its node, and a deny list refuses certificates and nodes
//!   ([`identity`]). The same listener serves daemons `ByteStream` for their blob
//!   reads and writes ([`blobs`]), each call admitted by the same rules.
//! - The operator API ([`api`]): HTTP/JSON under `/v1` on a third listener, off unless
//!   `--api-listen` is given; `GET /v1/nodes`, and cordon, drain and uncordon, which
//!   need the token of `--api-token-file` ([`token`]).
//! - Rollouts ([`rollout`]): the record's store (in memory for now) and the driver that
//!   cordons, drains and hands drained nodes their update, as far as `updating`.
//! - The MDM gate ([`mdm`]): the server's verbs at `kbf-mdm-gate` (inventory, enforce
//!   and withdraw a macOS build, install an allowlisted profile; no erase), a client
//!   over mutual TLS, and the polling of an update's DDM progress. Not yet called by
//!   the rollout driver.
//! - The [`farm::Farm`] core: `kbf-sched` decides, the farm carries out its effects.
//!   `Start` only after the grant commits; a result is accepted only from the node
//!   holding the operation's current lease, and only an accepted result is written to
//!   the action cache, before the callers are answered. Clients never write it.
//!
//! - REAPI client principals ([`principal`]): the token file format, its file rules
//!   and reload, and `kbf-server hash-token`. No listener reads it yet.
//!
//! Not yet: Raft (the control log is in-process: a record commits as soon as it is
//! appended, see [`farm`]), authentication of REAPI clients, operator roles (the
//! API's writes need one token, [`token`]), the `x-kbf-qos` header, learned sizes,
//! capability matching, and the daemon-side `ResultAck` handling (issue #26).

pub mod api;
pub mod blobs;
#[cfg(test)]
mod build_commit;
pub mod config;
pub mod farm;
pub mod fleet;
pub mod identity;
pub mod mdm;
pub mod principal;
pub mod rollout;
pub mod serve;
mod stamp;
pub mod token;
pub mod worker;

pub use config::{Args, Command, ConfigError, Role, StoreKind};
pub use farm::Farm;
pub use identity::{DenyList, DenyListError, Peers};
pub use serve::{Api, Bound, Listeners, ServeError, WorkerTls, bind_server, bind_server_with_api};
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
