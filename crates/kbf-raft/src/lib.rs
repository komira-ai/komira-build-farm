//! The replicated log: a sans-IO Raft core (see `docs/adr/0001-consensus.md`).
//!
//! [`Raft`] is one server of one group. It holds the term, the vote, the log and the
//! commit index, and it decides; it never reads a clock, draws a random number, starts
//! a thread or touches a disk or socket. The caller delivers ticks, messages and
//! commands, each tick or message with 64 bits of entropy for the election timeout,
//! and carries out the returned [`Effect`]s in order: persist the vote and entries,
//! send messages, apply committed entries. A process may hold any number of cores,
//! one per group.
//!
//! In scope here: leader election with randomized timeouts, log replication with the
//! append consistency check, commit by a majority of voters for entries of the current
//! term only, and learners that receive and apply the log without voting. Snapshots,
//! membership changes, PreVote and CheckQuorum come later; the log is already kept
//! relative to a snapshot base so snapshots need no change to its users.
//!
//! The specification is the Raft paper (<https://raft.github.io/raft.pdf>) and Diego
//! Ongaro's dissertation. The crate keeps the pure-crate rules (no clock, sleep,
//! network or hashed collections; see `clippy.toml`).

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod log;
mod message;
mod raft;
mod types;

pub use message::{AppendOutcome, Effect, Message, MessageKind};
pub use raft::{NotLeader, Proposed, Raft, Role};
pub use types::{
    Config, ConfigError, Entry, HardState, LogId, LogIndex, Membership, Payload, ServerId, Term,
};
