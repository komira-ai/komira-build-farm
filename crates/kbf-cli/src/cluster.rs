//! `kbf cluster status|drain|undrain`: a thin client of the operator API
//! (`docs/api.md`) on the frontend's host. It holds no cluster logic: each verb is one
//! call, and the answer is the API's JSON, printed as it came.
//!
//! - `kbf cluster status`: `GET /v1/nodes`, every node and where it is in placement.
//!   `--frontend`: `GET /v1/frontend/drain` instead, `{draining, open_executions,
//!   queued_operations}`.
//! - `kbf cluster drain <node> [--deadline-secs N]`: `POST /v1/nodes/<node>:drain`, the
//!   node takes no new lease and its leases run on. `kbf cluster drain --frontend`:
//!   `POST /v1/frontend/drain`, the frontend refuses new Execute calls, goes out of
//!   `/readyz`, and exits once its open streams are done.
//! - `kbf cluster undrain <node>`: `POST /v1/nodes/<node>:uncordon`. A frontend drain
//!   is not undone (the REAPI listener has said GOAWAY): restart it.
//! - `kbf cluster member <id> --state active|draining`: `POST /v1/members/<id>`, sets a
//!   storage member's state; this is how a `joining` member is promoted.
//!
//! Writes need `--token-file`, the token file the server was started with. The API
//! answers writes from loopback only, so this runs on the frontend's own host. Exit 0
//! for a 2xx answer, 1 for any other answer or no answer (the error goes to stderr), 2
//! for a command line that does not parse.

use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand, ValueEnum};
use kbf_server::api::{FRONTEND_DRAIN, NODES};
use kbf_server::api_client::{ClientError, OperatorClient, Reply};

/// The operator API's address when `--api` is not given: where the unit templates put
/// `--api-listen`.
pub const DEFAULT_API: &str = "127.0.0.1:8982";

/// `kbf cluster`'s command line.
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
}

/// One call to the API: the method, the path and the body of a [`Verb`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// `GET path`.
    Get(String),
    /// `POST path` with the JSON body (empty for none).
    Post(String, String),
}

impl Verb {
    /// The call this verb makes.
    #[must_use]
    pub fn call(&self) -> Call {
        match self {
            Self::Status { frontend: true } => Call::Get(FRONTEND_DRAIN.to_owned()),
            Self::Status { frontend: false } => Call::Get(NODES.to_owned()),
            Self::Drain { frontend: true, .. } => {
                Call::Post(FRONTEND_DRAIN.to_owned(), String::new())
            }
            Self::Drain {
                node,
                deadline_secs,
                ..
            } => {
                let body = deadline_secs
                    .map(|secs| format!("{{\"deadline_secs\":{secs}}}"))
                    .unwrap_or_default();
                let node = node.as_deref().unwrap_or_default();
                Call::Post(kbf_server::api::node_path(node, "drain"), body)
            }
            Self::Undrain { node, .. } => {
                Call::Post(kbf_server::api::node_path(node, "uncordon"), String::new())
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
                    Some(secs) => format!("{{\"state\":\"{state}\",\"for_secs\":{secs}}}"),
                    None => format!("{{\"state\":\"{state}\"}}"),
                };
                Call::Post(format!("/v1/members/{id}"), body)
            }
        }
    }

    /// The token file the verb's write needs, if it writes.
    #[must_use]
    pub fn token_file(&self) -> Option<&PathBuf> {
        match self {
            Self::Status { .. } => None,
            Self::Drain { token_file, .. }
            | Self::Undrain { token_file, .. }
            | Self::Member { token_file, .. } => Some(token_file),
        }
    }
}

/// Runs the verb against the API: the answer's body on stdout when it is 2xx (exit 0),
/// else the reason on stderr (exit 1).
#[must_use]
pub fn run(cluster: &Cluster) -> ExitCode {
    match answer(cluster) {
        Ok(reply) if reply.is_success() => {
            let _ = writeln!(std::io::stdout(), "{}", reply.body.trim_end());
            ExitCode::SUCCESS
        }
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
/// The token file cannot be read, or the API cannot be reached or answers nothing that
/// can be read.
pub fn answer(cluster: &Cluster) -> Result<Reply, ClientError> {
    let token = cluster
        .verb
        .token_file()
        .map(|path| OperatorClient::read_token(path))
        .transpose()?;
    let client = OperatorClient::new(cluster.api, token);
    match cluster.verb.call() {
        Call::Get(path) => client.get(&path),
        Call::Post(path, body) => client.post(&path, &body),
    }
}
