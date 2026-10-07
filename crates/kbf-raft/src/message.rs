//! What Raft servers send each other, and what the core asks its caller to do.

use crate::{Entry, HardState, LogId, LogIndex, ServerId, Term};

/// A message between two servers of one group. Every message carries the sender's
/// term: a receiver that sees a higher term adopts it and becomes a follower, and
/// a receiver that sees a lower term answers with its own so the sender catches up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// The sender's current term.
    pub term: Term,
    /// What it says.
    pub kind: MessageKind,
}

/// The four Raft messages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageKind {
    /// A candidate asks for a vote.
    VoteRequest {
        /// The candidate's last entry, for the up-to-date check.
        last_log: LogId,
    },
    /// The answer to a [`MessageKind::VoteRequest`].
    VoteResponse {
        /// Whether the vote was granted.
        granted: bool,
    },
    /// A leader sends entries (or none, as a heartbeat).
    AppendRequest {
        /// The entry just before `entries`; the receiver must hold it to accept.
        prev: LogId,
        /// The entries after `prev`, in order.
        entries: Vec<Entry>,
        /// The leader's commit index.
        commit: LogIndex,
    },
    /// The answer to a [`MessageKind::AppendRequest`].
    AppendResponse {
        /// Whether the receiver accepted.
        outcome: AppendOutcome,
    },
}

/// How a follower answered an append request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The follower's log matches the leader's up to and including `matched`, and those
    /// entries are durable.
    Accepted {
        /// The last index the request proved to match.
        matched: LogIndex,
    },
    /// The follower does not hold the request's `prev` entry.
    Rejected {
        /// The `prev` index of the refused request.
        at: LogIndex,
        /// The follower's last index, so the leader can skip back past a short log.
        last: LogIndex,
    },
}

/// Something the core asks its caller to do.
///
/// The caller carries out a list of effects **in order**, and a later effect may rely
/// on an earlier one: a [`Effect::Send`] must not leave until every persist effect
/// before it is durable, because the message may promise that state (a granted vote,
/// an accepted entry). [`Effect::Apply`] entries are committed and arrive in index
/// order, each once per core instance; a core restored after a restart applies again
/// from the start of its log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Make the term and vote durable.
    PersistHardState(HardState),
    /// Make `entries` durable: drop every persisted entry at or after the index of the
    /// first one, then append them. Never empty.
    PersistEntries(Vec<Entry>),
    /// Send `msg` to `to`.
    Send {
        /// The receiver.
        to: ServerId,
        /// The message.
        msg: Message,
    },
    /// Apply a committed entry to the state machine.
    Apply(Entry),
}
