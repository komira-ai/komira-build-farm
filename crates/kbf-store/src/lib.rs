//! A Raft server's durable state on the local disk: the log and the hard state (term
//! and vote) that [`kbf_raft`]'s persist effects ask for.
//!
//! - The log is a series of append-only segment files of CRC-checked records
//!   (the `record` module has the layout). An append that drops entries (a
//!   follower's log meeting a new leader's) is written as a new segment and never
//!   rewrites a file in place.
//! - The hard state is one record, written to a temporary file, synced, renamed over
//!   the old one, and the directory synced.
//! - At open, a torn tail (a damaged record at the end of the last segment, with no
//!   whole record after it) is cut off: only bytes past the last sync can be torn,
//!   and nothing past the last sync was acknowledged. Damage anywhere else refuses the
//!   open.
//! - **A write or sync error stops the store.** It returns the error and then refuses
//!   every later call; it never acknowledges anything it could not make durable.
//!
//! All disk access goes through the [`Fs`] trait. [`StdFs`] is the local disk;
//! [`FaultFs`] is an in-memory directory that fails or crashes at any chosen
//! operation and shows what a power cut would leave, which the tests use to crash the
//! store at every operation and tear every write at every byte.
//!
//! Each call syncs before it returns; batching several persists into one sync is the
//! caller's choice of how many entries to pass at once.

mod crc;
mod fault;
mod fs;
mod record;
mod store;

#[cfg(test)]
mod tests;

pub use crate::fault::{Fault, FaultFile, FaultFs};
pub use crate::fs::{Fs, FsFile, StdFile, StdFs};
pub use crate::store::{HARD_STATE, HARD_STATE_TMP, Options, Recovered, Store, StoreError};
