//! What [`super::Controller::tick`] answers: what the tick did, and why it could not.

use kbf_types::WorkerId;

use super::command::Command;
use super::node_step::HostPhase;
use super::pick::WaitReason;
use super::policy::PolicyError;
use super::ports::{SourceError, TransportError};
use super::rows::RowError;

/// Why a tick could not be made. The rows are as they were; the next tick starts again.
#[derive(Debug, thiserror::Error)]
pub enum ControllerError {
    /// The rows could not be read or written.
    #[error(transparent)]
    Rows(#[from] RowError),
    /// The farm could not be read.
    #[error("the farm could not be read: {0}")]
    Source(#[from] SourceError),
    /// `update/policy` cannot be kept: nothing moves until it is replaced.
    #[error("update/policy cannot be kept: {0}")]
    Policy(#[from] PolicyError),
    /// The clock reads earlier than a time the rows hold, so it restarted below what it was
    /// (see "Time" in the docs of [`super::Controller`]). Every limit counts from a stored
    /// time; against a clock that is behind them none would ever pass.
    #[error("the clock reads {now_ms} ms, before the {latest_ms} ms a row holds")]
    ClockBehind {
        /// The time of this tick, in milliseconds.
        now_ms: u64,
        /// The latest time a row holds, in milliseconds.
        latest_ms: u64,
    },
    /// A row holds an order of a later epoch than this controller's, so this controller's
    /// epoch is not above every epoch the rows were written under
    /// ([`super::Controller::new`]) and the ids it draws could repeat one a host has seen.
    #[error("{row} holds an order of epoch {found}, above this controller's epoch {epoch}")]
    EpochBehind {
        /// The row.
        row: String,
        /// The epoch it holds.
        found: u64,
        /// This controller's epoch.
        epoch: u64,
    },
    /// The controller disagrees with itself. A bug, reported rather than guessed past.
    #[error("the controller broke its own rule: {0}")]
    Invariant(String),
}

/// A host that changed phase in a tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Moved {
    /// The host.
    pub host: WorkerId,
    /// The phase it left.
    pub from: HostPhase,
    /// The phase it entered.
    pub to: HostPhase,
}

/// An order the farm did not take. It is given again at the next tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandError {
    /// The host the order was for.
    pub host: WorkerId,
    /// The order.
    pub command: Command,
    /// What went wrong.
    pub error: TransportError,
}

/// What one tick did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// The hosts that changed phase, in the order they were written.
    pub moved: Vec<Moved>,
    /// The hosts started (a subset of `moved`: they entered `Selected`).
    pub started: Vec<WorkerId>,
    /// Why no host was started, when the picker was asked and said so.
    pub waiting: Option<WaitReason>,
    /// The orders the farm did not take.
    pub errors: Vec<CommandError>,
    /// A row had been moved by another controller: the tick stopped, nothing was overwritten.
    pub conflict: bool,
    /// Hosts the rows hold in a step that the farm does not show. They are not moved, and
    /// no host is started while there are any.
    pub missing: Vec<WorkerId>,
}
