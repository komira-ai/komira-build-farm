//! The metadata core: blob and action-cache records, the closure check, and the
//! present, absent and unavailable answers, as a pure state machine.
//!
//! [`MetaState`] holds what the farm knows about stored content:
//! - the **CAS index**: each blob digest maps to its [`Location`] (a record in a
//!   segment object of a store; a large blob is a segment of one record) and the farm
//!   time of its last touch;
//! - the **action cache**: each action digest maps to an [`ActionRecord`], the
//!   `ActionResult` blob plus its [`Closure`];
//! - the objects the store has reported unreachable, each with a [`LossMark`]: its
//!   [`UnreachableReason`] and the [`Generation`] (log index) of the last mark;
//! - the committed farm time, and the next writer [`Epoch`].
//!
//! Object ids are a writer epoch and a sequence number ([`ObjectId`]). Epochs come only
//! from [`Command::AllocEpoch`], never twice, and a command naming an object of an epoch
//! never allocated is refused ([`UnallocatedEpoch`]).
//!
//! It changes only by [`Command`]s, which every replica applies in log order, each with
//! the log index of its entry ([`MetaState::execute`]); farm time arrives as
//! [`Command::Tick`]. Reads are queries that answer from the state:
//! - a blob is [`BlobAnswer::Present`], [`BlobAnswer::Unavailable`] (held, but its
//!   object is unreachable) or [`BlobAnswer::Absent`]. Only `Absent` becomes NOT_FOUND;
//! - an action is a hit only after the closure check finds the result blob and every
//!   closure blob present and reachable ([`ActionAnswer`]);
//! - a read that reports or serves something returns a [`Touch`] to commit first, which
//!   keeps the retention promise of [`Retention`].
//!
//! Only a daemon may write the action cache ([`Role`]), and only once every blob the
//! entry needs is held.
//!
//! This is a pure crate: no async runtime, network, clock, randomness or hashed
//! collections. The layering test in `kbf-it` and the lists in `clippy.toml` enforce it.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

mod model;
mod state;

pub use crate::model::{
    ActionAnswer, ActionRecord, BlobAnswer, Closure, Epoch, FindMissing, Generation, Location,
    LossMark, Miss, ObjectId, Retention, Role, StoreId, Touch, UnreachableReason,
};
pub use crate::state::{
    ActionWriteError, Applied, BlobWrite, Collected, Command, MetaState, UnallocatedEpoch,
};
