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
//!   an object marked while it was staged has lost its blobs (their only copy was its
//!   block on a write tier), counted by [`MetaState::lost_blobs`] and
//!   [`MetaState::blob_losses`];
//! - the committed farm time, the next writer [`Epoch`], and the node writers set
//!   ([`NodeWriter`]), the latest of them current;
//! - the local write tiers ([`TierId`]), one current tier per server, and the
//!   **staged** objects: objects whose only copy is a block file on a tier until they
//!   are flushed to the store ([`StagedObject`], docs/design/write-path.md, section 6).
//!   A staged blob answers `Present` with the tier it is staged on.
//!
//! Object ids are a [`Writer`] and a sequence number ([`ObjectId`]). A writer is an
//! epoch, which comes only from [`Command::AllocEpoch`], never twice, or a registered
//! storage node's member and boot ([`NodeWriter`]), which the state store hands out and
//! [`Command::SetWriter`] makes current. A command naming an object of an epoch never
//! allocated or of a node writer never set is refused, and so is a put naming a node
//! writer that is not the current one ([`ForeignWriter`]).
//! An object some other writer put (another node, an earlier boot, an epoch of a log
//! that was lost) enters the index only by [`Command::AdoptSegment`], the step of a
//! rebuild of the index from the store, which records a segment's records that are not
//! indexed yet and never replaces one that is; [`Command::RebuildProgress`] and
//! [`Command::RebuildDone`] keep how far that rebuild is ([`MetaState::rebuild`]).
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
mod tier;

pub use crate::model::{
    ActionAnswer, ActionRecord, BlobAnswer, Closure, Epoch, FindMissing, Generation, LegacyWriter,
    Location, LossMark, Miss, NodeWriter, ObjectId, Retention, Role, StoreId, Touch,
    UnreachableReason, Writer,
};
pub use crate::state::{
    ActionWriteError, AdoptRefused, Adopted, Applied, BlobWrite, Collected, Command, Evicted,
    ForeignWriter, MetaState, Rebuild, StaleWriter,
};
pub use crate::tier::{ServerId, StagedObject, StagedPut, StagedState, TierError, TierId};
