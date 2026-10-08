//! `kbf-server`: the farm server, single node. It wires the front, the scheduler, the
//! metadata core and storage together; the decisions are theirs.
//!
//! - REAPI on one listener: the cache services and `Execution` from `kbf-front`, over
//!   a [`kbf_front::Cache`] (in-memory metadata, and an in-memory or S3 object store).
//! - `kbf.worker.v1` on another ([`worker`]): daemons register, heartbeat, receive
//!   lease offers and `Start`s, and report results. Under mutual TLS a daemon's
//!   certificate must name its node, and a deny list refuses certificates and nodes
//!   ([`identity`]).
//! - The [`farm::Farm`] core: `kbf-sched` decides, the farm carries out its effects.
//!   `Start` only after the grant commits; a result is accepted only from the node
//!   holding the operation's current lease, and only an accepted result is written to
//!   the action cache, before the callers are answered. Clients never write it.
//!
//! Not yet: Raft (the control log is in-process: a record commits as soon as it is
//! appended, see [`farm`]), authentication, the `x-kbf-qos` header, learned sizes,
//! capability matching, and the daemon-side `ResultAck` handling (issue #26).

pub mod config;
pub mod farm;
pub mod identity;
pub mod serve;
pub mod worker;

pub use config::{Args, ConfigError, Role, StoreKind};
pub use farm::Farm;
pub use identity::{DenyList, DenyListError, Peers};
pub use serve::{Bound, Listeners, ServeError, WorkerTls, bind_server};
pub use worker::WorkerService;
