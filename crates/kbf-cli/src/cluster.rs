//! `kbf cluster status|drain|undrain|member|safe-to-stop|denied|epoch-bump|object-store`: a thin client of the operator API
//! (`docs/api.md`) on the frontend's host. It holds no cluster logic: each verb is one
//! call, and the answer is the API's JSON, printed as it came.
//!
//! - `kbf cluster status`: `GET /v1/nodes`, every node and where it is in placement.
//!   `--frontend`: `GET /v1/frontend/drain` instead, `{draining, phase, open_executions,
//!   queued_operations}`.
//! - `kbf cluster drain <node> [--deadline-secs N]`: `POST /v1/nodes/<node>:drain`, the
//!   node takes no new lease and its leases run on. `kbf cluster drain --frontend`:
//!   `POST /v1/frontend/drain`, the frontend goes out of `/readyz` at once, refuses new
//!   Execute calls after `--drain-lb-wait`, and exits once its open executions are done
//!   (at most `--drain-deadline`).
//! - `kbf cluster undrain <node>`: `POST /v1/nodes/<node>:uncordon`. A frontend drain
//!   is not undone (the process exits): start a new one.
//! - `kbf cluster member <id> --state active|draining`: `POST /v1/members/<id>`, sets a
//!   storage member's state; this is how a `joining` member is promoted.
//! - `kbf cluster safe-to-stop <id>`: `GET /v1/members/<id>/safe-to-stop`, whether the host of
//!   a storage member may be stopped now. A read, but it passes the whole write gate, so it
//!   needs `--token-file`. Exit 0 if it may, 3 if not (the reasons on stdout), 1 if the call
//!   failed.
//! - `kbf cluster denied`: `GET /v1/denied`, the storage deny list with its reasons.
//! - `kbf cluster epoch-bump`: `POST /v1/epoch/bump`, raises the epoch the state store
//!   holds by one, for a store restored from a backup (the frontends that start after it
//!   serve the new epoch, so the daemons drop the leases the lost history granted; a
//!   frontend that runs keeps the one it started under). The answer names the epoch
//!   before, the epoch stored now and the epoch the frontend serves.
//! - `kbf cluster object-store ...`: the object-store registration routes
//!   ([`object_store`]). `denied` and `object-store` pass the whole write gate with a
//!   `GET`, so they need `--token-file` too.
//!
//! Writes need `--token-file`, the token file the server was started with. The API
//! answers writes from loopback only, so this runs on the frontend's own host. Exit 0
//! for a 2xx answer, 1 for any other answer or no answer (the error goes to stderr), 2
//! for a command line that does not parse; `safe-to-stop` exits 3 for a 2xx answer that
//! says the stop is not safe.

pub mod object_store;
mod safe_to_stop;

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand, ValueEnum};
use kbf_server::api::{DENIED, EPOCH_BUMP, FRONTEND_DRAIN, NODES, safe_to_stop_path};
use kbf_server::api_client::{ClientError, OperatorClient, Reply};
use serde::Serialize;
use zeroize::Zeroizing;

/// The operator API's address when `--api` is not given: where the unit templates put
/// `--api-listen`.
pub const DEFAULT_API: &str = "127.0.0.1:8982";

/// Show, drain and undrain the cluster, set a member's state, ask whether a member's host may
/// be stopped, list the denied hosts, raise the state store's epoch and manage the object
/// store, through the frontend's operator API.
#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct Cluster {
    /// The operator API (the server's `--api-listen`).
    #[arg(long, default_value = DEFAULT_API, global = true)]
    pub api: SocketAddr,
    #[command(subcommand)]
    pub verb: Verb,
}

/// The states `kbf cluster member` sets: the ones the API accepts.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberState {
    /// A full owner of its ranks.
    Active,
    /// Takes no new writes.
    Draining,
    /// Out of the write path for a planned restart, at once; `active` undoes it.
    #[value(name = "planned-down")]
    PlannedDown,
}

/// What `kbf cluster` does.
#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Show every node's placement (or, with --frontend, the frontend's drain).
    Status {
        /// The frontend's drain: whether it is draining, and the streams and queued
        /// operations left.
        #[arg(long)]
        frontend: bool,
    },
    /// Drain a node, or with --frontend the frontend.
    #[command(group(clap::ArgGroup::new("what").required(true).args(["node", "frontend"])))]
    Drain {
        /// The node to drain: no new lease; its leases run on.
        node: Option<String>,
        /// Drain the frontend: no new Execute, out of /readyz, exits when its streams
        /// are done. Not undone.
        #[arg(long, conflicts_with = "deadline_secs")]
        frontend: bool,
        /// How long the node's drain waits for its leases (the server's default, 30
        /// minutes, if not given).
        #[arg(long, value_name = "SECS")]
        deadline_secs: Option<u64>,
        /// The file holding the server's operator API token.
        #[arg(long, value_name = "FILE")]
        token_file: PathBuf,
    },
    /// Return a drained node to placement.
    Undrain {
        /// The node.
        node: String,
        /// The file holding the server's operator API token.
        #[arg(long, value_name = "FILE")]
        token_file: PathBuf,
    },
    /// Set a storage member's state (see `GET /v1/members` for the ids).
    Member {
        /// The member id the store assigned at registration.
        id: u64,
        /// The state to set. `active` promotes a joining member to a full owner.
        #[arg(long, value_enum)]
        state: MemberState,
        /// With `planned-down`: how long the restart may last before the member returns by
        /// its next heartbeat (default 15 minutes, at most a day).
        #[arg(long, value_name = "SECONDS")]
        for_secs: Option<u64>,
        /// The file holding the server's operator API token.
        #[arg(long, value_name = "FILE")]
        token_file: PathBuf,
    },
    /// Ask whether a storage member's host may be stopped now: exit 0 if it may, 3 if not
    /// (the reasons are printed).
    SafeToStop {
        /// The storage member's number (see `GET /v1/members`), or the node id of its host;
        /// the node id of a host with no storage member is safe, judged as a daemon.
        id: String,
        /// The file holding the server's operator API token.
        #[arg(long, value_name = "FILE")]
        token_file: PathBuf,
    },
    /// List the storage deny list with the reason and time of each entry.
    Denied {
        #[command(flatten)]
        token: object_store::Token,
    },
    /// Raise the epoch the state store holds by one, after a restore from a backup: the
    /// frontends that start after it serve the new epoch, so the daemons drop the leases the
    /// lost history granted. The frontend that answers goes on serving its own until it is
    /// restarted.
    EpochBump {
        /// The file holding the server's operator API token.
        #[arg(long, value_name = "FILE")]
        token_file: PathBuf,
    },
    /// Register the farm's object store: connections, the `cold` binding, probes, status
    /// and audit.
    ObjectStore {
        #[command(subcommand)]
        verb: object_store::ObjectStore,
    },
}

/// A request body that may hold a secret: zeroed when dropped, and its `Debug` shows
/// nothing, so a [`Call`] printed with `{:?}` leaks no key.
#[derive(Clone, PartialEq, Eq)]
pub struct Body(Zeroizing<String>);

impl Body {
    /// No body.
    #[must_use]
    pub fn empty() -> Self {
        Self(Zeroizing::new(String::new()))
    }

    /// The JSON of `value`.
    ///
    /// # Panics
    /// `value` does not serialize to JSON (a map with non-string keys): the types
    /// passed here are fixed structs.
    #[must_use]
    pub fn json(value: &impl Serialize) -> Self {
        // Written straight into a zeroed buffer: no unzeroed copy of the key is made here.
        let mut bytes = Zeroizing::new(Vec::new());
        serde_json::to_writer(&mut *bytes, value).expect("a fixed struct serializes");
        Self(Zeroizing::new(
            String::from_utf8(std::mem::take(&mut *bytes)).expect("JSON is UTF-8"),
        ))
    }

    /// The body's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Body(..)")
    }
}

/// Why a verb made no call, or got no answer.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    /// The token file, the connection, or the answer: see [`ClientError`].
    #[error(transparent)]
    Client(#[from] ClientError),
    /// The secret key could not be read from its source. The text names the source and
    /// what is wrong with it, never a byte of what was read.
    #[error("{origin}: {why}")]
    Secret {
        /// `standard input`, or `secret file <path>`.
        origin: String,
        /// What is wrong with it.
        why: String,
    },
}

/// One call to the API: the method, the path and the body of a [`Verb`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// `GET path`, as a read of the status routes sends it.
    Get(String),
    /// `GET path` through the whole write gate: the bearer and the JSON content type.
    GetGated(String),
    /// `POST path` with the JSON body (empty for none).
    Post(String, Body),
    /// `PUT path` with the JSON body.
    Put(String, Body),
    /// `DELETE path`.
    Delete(String),
}

impl Verb {
    /// The call this verb makes. `stdin` is read only by `object-store connection put
    /// --secret-stdin`.
    ///
    /// # Errors
    /// The secret key of `connection put` cannot be read from its source.
    pub fn call(&self, stdin: &mut dyn std::io::Read) -> Result<Call, CallError> {
        Ok(match self {
            Self::Status { frontend: true } => Call::Get(FRONTEND_DRAIN.to_owned()),
            Self::Status { frontend: false } => Call::Get(NODES.to_owned()),
            Self::Drain { frontend: true, .. } => {
                Call::Post(FRONTEND_DRAIN.to_owned(), Body::empty())
            }
            Self::Drain {
                node,
                deadline_secs,
                ..
            } => {
                let body = deadline_secs.map_or_else(Body::empty, |secs| {
                    Body::json(&serde_json::json!({ "deadline_secs": secs }))
                });
                let node = node.as_deref().unwrap_or_default();
                Call::Post(kbf_server::api::node_path(node, "drain"), body)
            }
            Self::Undrain { node, .. } => {
                Call::Post(kbf_server::api::node_path(node, "uncordon"), Body::empty())
            }
            Self::Member {
                id,
                state,
                for_secs,
                ..
            } => {
                let state = match state {
                    MemberState::Active => "active",
                    MemberState::Draining => "draining",
                    MemberState::PlannedDown => "planned_down",
                };
                let body = match for_secs {
                    Some(secs) => serde_json::json!({ "state": state, "for_secs": secs }),
                    None => serde_json::json!({ "state": state }),
                };
                Call::Post(format!("/v1/members/{id}"), Body::json(&body))
            }
            Self::SafeToStop { id, .. } => Call::GetGated(safe_to_stop_path(id)),
            Self::Denied { .. } => Call::GetGated(DENIED.to_owned()),
            Self::EpochBump { .. } => Call::Post(EPOCH_BUMP.to_owned(), Body::empty()),
            Self::ObjectStore { verb } => verb.call(stdin)?,
        })
    }

    /// The token file the verb's write needs, if it writes.
    #[must_use]
    pub fn token_file(&self) -> Option<&PathBuf> {
        match self {
            Self::Status { .. } => None,
            Self::Drain { token_file, .. }
            | Self::Undrain { token_file, .. }
            | Self::Member { token_file, .. }
            | Self::SafeToStop { token_file, .. }
            | Self::EpochBump { token_file } => Some(token_file),
            Self::Denied { token } => Some(&token.token_file),
            Self::ObjectStore { verb } => Some(verb.token_file()),
        }
    }
}

/// Prints a 2xx answer of `verb` and returns the exit status it means: 0, or for
/// `safe-to-stop` the verdict's ([`safe_to_stop::report`]).
fn succeeded(verb: &Verb, reply: &Reply) -> ExitCode {
    if let Verb::SafeToStop { id, .. } = verb {
        let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
        return ExitCode::from(safe_to_stop::report(id, reply, &mut out, &mut err));
    }
    let _ = writeln!(std::io::stdout(), "{}", reply.body.trim_end());
    ExitCode::SUCCESS
}

/// Runs the verb against the API: the answer's body on stdout when it is 2xx (exit 0),
/// else the reason on stderr (exit 1). `safe-to-stop` prints the verdict instead and exits
/// 3 when the stop is not safe.
#[must_use]
pub fn run(cluster: &Cluster) -> ExitCode {
    match answer(cluster) {
        Ok(reply) if reply.is_success() => succeeded(&cluster.verb, &reply),
        Ok(reply) => {
            eprintln!("kbf cluster: {} {}", reply.status, reply.body.trim_end());
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("kbf cluster: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The API's answer to the verb.
///
/// # Errors
/// The token file or the secret key cannot be read, or the API cannot be reached or
/// answers nothing that can be read.
pub fn answer(cluster: &Cluster) -> Result<Reply, CallError> {
    let token = cluster
        .verb
        .token_file()
        .map(|path| OperatorClient::read_token(path))
        .transpose()?;
    let client = OperatorClient::new(cluster.api, token);
    let call = cluster.verb.call(&mut std::io::stdin().lock())?;
    Ok(match call {
        Call::Get(path) => client.get(&path)?,
        Call::GetGated(path) => client.get_gated(&path)?,
        Call::Post(path, body) => client.post(&path, body.as_str())?,
        Call::Put(path, body) => client.put(&path, body.as_str())?,
        Call::Delete(path) => client.delete(&path)?,
    })
}
