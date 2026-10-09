//! An action's outputs, read from the directory it ran in, and that directory removed
//! afterwards; both without following a symlink at any level.
//!
//! - [`collect`] reads each declared output into a [`Store`] and records it in an
//!   `ActionResult`: every component is opened relative to its parent's descriptor
//!   with `O_NOFOLLOW`, an output directory is walked iteratively within
//!   [`OutputLimits`] (depth, entries, bytes), a file larger than one chunk is hashed
//!   and then uploaded from its descriptor ([`Store::put_file`]) rather than held in
//!   memory, and every name must be UTF-8 (REAPI names are strings; a lossy decode
//!   could make two names one).
//! - [`remove_tree`] removes a directory tree iteratively, by descriptor, restoring
//!   the owner's permissions on any directory the action locked and, on macOS,
//!   clearing the immutable and append-only user flags (`chflags uchg`, `uappnd`) it
//!   set on any entry and every ACL, never following a symlink; [`remove_tree_at`]
//!   does the same for a name in a directory the caller holds open.
//!
//! The native driver (`kbf-driver-native`) uses both. The container driver
//! (`kbf-driver-container`) keeps its own copy of the walk for now; moving it onto
//! this crate is a follow-up.
//!
//! Callers must end every process of the action first: a process left running could
//! swap a directory between a check and a read. The walk still refuses what such a
//! swap would leave (a `..` that is not the directory it came from, a file that is no
//! longer regular once opened), but it does not race a live writer to the end.

mod collect;
mod limits;
mod remove;
mod store;

pub use collect::{OutputsError, collect, store_file};
pub use limits::{Exceeded, OutputLimits};
pub use remove::{remove_tree, remove_tree_at};
pub use store::{CHUNK_BYTES, Store, StoreError, digest_of};
