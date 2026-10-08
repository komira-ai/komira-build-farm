//! `kbf-server`: the farm server, single node. It wires the front, the scheduler, the
//! metadata core and storage together; the decisions are theirs.
//!
//! - REAPI on one listener: the cache services and `Execution` from `kbf-front`, over
//!   a [`kbf_front::Cache`] (in-memory metadata, and an in-memory or S3 object store).
//! - `kbf.worker.v1` on another ([`worker`]): daemons register, heartbeat, receive
//!   lease offers and `Start`s, and report results. Under mutual TLS a daemon's
//!   certificate must name its node, and a deny list refuses certificates and nodes
//!   ([`identity`]).
//! - The operator API ([`api`]): HTTP/JSON under `/v1` on a third listener, off unless
//!   `--api-listen` is given; `GET /v1/nodes`, and cordon, drain and uncordon, which
//!   need the token of `--api-token-file` ([`token`]).
//! - Rollouts ([`rollout`]): the record's store (in memory for now) and the driver that
//!   cordons, drains and hands drained nodes their update, as far as `updating`.
//! - The [`farm::Farm`] core: `kbf-sched` decides, the farm carries out its effects.
//!   `Start` only after the grant commits; a result is accepted only from the node
//!   holding the operation's current lease, and only an accepted result is written to
//!   the action cache, before the callers are answered. Clients never write it.
//!
//! Not yet: Raft (the control log is in-process: a record commits as soon as it is
//! appended, see [`farm`]), authentication of REAPI clients, operator roles (the
//! API's writes need one token, [`token`]), the `x-kbf-qos` header, learned sizes,
//! capability matching, and the daemon-side `ResultAck` handling (issue #26).

pub mod api;
pub mod config;
pub mod farm;
pub mod fleet;
pub mod identity;
pub mod rollout;
pub mod serve;
pub mod token;
pub mod worker;

pub use config::{Args, ConfigError, Role, StoreKind};
pub use farm::Farm;
pub use identity::{DenyList, DenyListError, Peers};
pub use serve::{Api, Bound, Listeners, ServeError, WorkerTls, bind_server, bind_server_with_api};
pub use worker::WorkerService;
