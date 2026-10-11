//! A Raft server's durable state on the local disk: the log, the hard state (term
//! and vote) and the newest snapshot that [`kbf_raft`]'s persist effects ask for.
//! [`Store`] implements [`kbf_raft::Storage`], so a [`kbf_raft::Host`] runs over it.
//!
//! - The log is a series of append-only segment files of CRC-checked records
//!   (the `record` module has the layout). An append that drops entries (a
//!   follower's log meeting a new leader's) is written as a new segment and never
//!   rewrites a file in place.
//! - The hard state is one record, written to a temporary file, synced, renamed over
//!   the old one, and the directory synced.
//! - The newest snapshot is one file, written the same way. Only once it is durable
//!   are the segments it covers removed; a segment that holds the snapshot's base and
//!   entries after it is kept whole, and the base the snapshot file records hides its
//!   entries through the base.
//! - At open, a torn tail (a damaged record at the end of the last segment, with no
//!   whole record after it) is cut off: only bytes past the last sync can be torn,
//!   and nothing past the last sync was acknowledged. Damage anywhere else refuses the
//!   open.
//! - **A write or sync error stops the store.** It returns the error and then refuses
//!   every later call; it never acknowledges anything it could not make durable.
//!   That stop lasts as long as the process: a reopen after a failed sync, before
//!   the machine restarts, can read bytes that never reached the disk
//!   ([`StoreError::Io`] says why).
//!
//! All disk access goes through the [`Fs`] trait. [`StdFs`] is the local disk;
//! [`FaultFs`] is an in-memory directory that fails or crashes at any chosen
//! operation and shows what a power cut would leave (including any subset of the name
//! changes since the last directory sync), which the tests use to crash the store at
//! every operation and tear every write at every byte.
//!
//! Each call syncs before it returns; batching several persists into one sync is the
//! caller's choice of how many entries to pass at once. So [`kbf_raft::Storage::sync`]
//! has nothing left to do, and [`kbf_raft::Storage::load`] reads the directory again.

mod crc;
mod fault;
mod fs;
mod record;
mod store;

#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
mod tests;

pub use crate::fault::{Fault, FaultFile, FaultFs};
pub use crate::fs::{Fs, FsFile, StdFile, StdFs};
pub use crate::store::{
    HARD_STATE, HARD_STATE_TMP, Options, Recovered, SNAPSHOT, SNAPSHOT_TMP, Store, StoreError,
};
