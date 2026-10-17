//! Build history: the object format, keys and retention classes of the history the
//! frontend keeps in its object store, the run ids that find it, and the writer and
//! reader cores over the `kbf-objstore` and `kbf-state` traits.
//!
//! The design is `docs/design/ui-data.md`, section 5 (storage of history), with run ids
//! in section 3. The object store is the truth for history; the `ui/` rows on the state
//! store are an index that can be rebuilt from the objects:
//!
//! - [`container`]: the one container format (header, zstd blocks of length-prefixed
//!   protobuf records, footer with the block index and the family summary, trailer).
//! - [`keys`]: UTC days, writer ids, invocation keys, retention [`Class`]es, object
//!   [`Family`]s, object keys and, under an [`IndexRoot`], index row keys.
//! - [`HistoryIndex`]: the state store under its [`IndexRoot`], every blocking call run
//!   through an [`Offload`] (the server's blocking pool, or [`Inline`]).
//! - [`records`]: run, hit and outcome records, tails, command renderings and the
//!   failed-run section, each with its `instance`; the index row values.
//! - [`run_id`]: time-ordered [`RunId`]s, stream blocks and the fallback range.
//! - [`Writer`]: events into chunks, object first and index row second, with bounded
//!   queues, drop counters and re-insertion of dropped rows with no LIST.
//! - [`Reader`]: a run id to its record; an invocation to its header and newest
//!   summary, and to its records a bounded page of chunks at a time.
//! - [`flog`]: failed runs' logs copied into `r30/flog` objects that outlive the CAS blobs,
//!   and [`Reader::failure_log`] to read a range of one back.
//! - [`Scoped`]: an object store that refuses every key outside `history/`.
//!
//! No clock is read here: every time is an argument.

#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod container;
pub mod flog;
mod index;
pub mod keys;
mod pending;
mod queue;
mod reader;
pub mod records;
pub mod run_id;
mod scoped;
mod writer;

pub use flog::{FlogObject, FlogParts, FlogRef, FlogStore};
pub use index::{HistoryIndex, Inline, Offload, Offloaded};
pub use keys::{Class, Day, Family, HistoryRoot, IndexPrefixError, IndexRoot, InvKey, WriterId};
pub use pending::PendingLimits;
pub use queue::{Event, EventQueue, InvRef, QueueLimits};
pub use reader::{
    Allow, ChunkCursor, CursorTextError, FailureLog, InvocationView, MAX_PAGE_CHUNKS, ReadError,
    Reader, RecordsPage, RunView,
};
pub use run_id::{Minted, Minter, RunId, StreamId};
pub use scoped::Scoped;
pub use writer::{Drops, Limits, SLOWEST, Writer};
