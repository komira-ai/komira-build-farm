//! The disk layer of the local write tier (design `docs/design/write-path.md`
//! section 6, rollout stage 4): block files in the hot store directory (`--hot-store`), written in reserved
//! ranges, synced before anything in them is acknowledged, sealed, counted against
//! the hot store's budget, and recovered at a restart from the committed state.
//!
//! - A **block** is one file, `<epoch>-<seq>.blk` (`<member>-<boot>-<seq>.blk` for a
//!   node writer), named by the object it becomes. A
//!   blob's offset in the block is its offset in that object. Grouped writes share
//!   the one open block, each in a range reserved in order ([`WriteTier::reserve_group`]);
//!   a segment upload's segment is a block of its own ([`WriteTier::reserve_upload`]).
//! - **Durability.** [`WriteTier::sync`] runs `fdatasync` on the block's handle after
//!   the range's positioned writes, and `sync_dir` once per new block, before it
//!   returns the [`Synced`] a `PutStaged` may name. A write or sync error poisons the
//!   block: no later sync of it succeeds (a failed sync may have consumed another
//!   range's write-back error), and the grouped block is sealed.
//! - **Answers.** A range is answered when its `PutStaged` commits
//!   ([`WriteTier::committed`]: the committed length becomes the largest committed end,
//!   so out-of-order commits never move it back) or fails (the reservation dropped: a
//!   grouped range's bytes stay as dead bytes; an upload's block is deleted).
//! - **Seal.** The open grouped block is sealed at the segment limit or after
//!   the seal-after interval (30 s): it takes no new range, and once every range is
//!   answered [`WriteTier::poll_seals`] names it for `SealStaged`. A block whose ranges
//!   all failed is deleted instead.
//! - **Budget.** Every live block's reserved extent counts, from reservation until
//!   [`WriteTier::unstage`] (after its flush) or deletion. A range that does not fit is
//!   refused with [`TierError::Full`]: the caller takes the direct path.
//! - **Reads** never pass a block's committed length: bytes no commit named were never
//!   acknowledged and are never served.
//! - **Restart** ([`Tier::open`]): the `TIER` file check, each staged object's block
//!   cut to its committed length (or reported lost), and every other block deleted
//!   (the orphan sweep). The committed state comes in through [`StagedIndex`].
//!
//! - **Flushed copies.** Once an object's `ObjectFlushed` committed,
//!   [`WriteTier::flushed`] renames its block to `<epoch>-<seq>.seg`. Copies are read
//!   like blocks and evicted, least recently read first, when a reservation needs
//!   their room: only `.seg` files are ever evicted, so a block whose only copy is
//!   local never is. [`WriteTier::unflushed`] measures what a disk loss would take.
//!
//! The flusher, reads through the metadata, the restart's metadata side and the
//! metrics are `kbf-front`'s (`Cache::attach_tier`, `Cache::flush_tier`). All disk
//! access goes through [`kbf_store::Fs`], so the tests crash it with
//! [`kbf_store::FaultFs`].

mod block;
mod error;
mod open;
mod tier;

pub use kbf_meta::TierId;

pub use crate::block::{TIER, block_name, parse_block_name, parse_seg_name, seg_name};
pub use crate::error::TierError;
pub use crate::open::{Recovery, StagedIndex, StagedObject, StagedSnapshot, TierStart, clear};
pub use crate::tier::{
    NextSeq, Reservation, Synced, Tier, TierOptions, Unflushed, Unsynced, Usage, WriteTier,
};
