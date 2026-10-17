//! A committed state for the tier tests: the staged table `PutStaged` and `SealStaged`
//! would build, and helpers to open a tier over a `FaultFs`.

#![allow(dead_code, reason = "each test binary uses part of it")]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use kbf_meta::{Epoch, ObjectId};
use kbf_store::{FaultFs, Fs};
use kbf_tier::{
    Recovery, StagedIndex, StagedObject, Synced, Tier, TierError, TierId, TierOptions, TierStart,
    WriteTier, block_name,
};

pub const TIER: TierId = TierId::new(1);
pub const SEAL_AFTER: Duration = Duration::from_secs(30);

/// The staged table, per tier.
#[derive(Clone, Debug, Default)]
pub struct Index {
    pub tiers: BTreeMap<TierId, BTreeMap<ObjectId, StagedObject>>,
}

impl StagedIndex for Index {
    fn staged_on(&self, tier: TierId) -> Vec<StagedObject> {
        self.tiers
            .get(&tier)
            .map(|t| t.values().copied().collect())
            .unwrap_or_default()
    }
}

impl Index {
    /// `PutStaged` for `synced`: the committed length becomes the larger end; an
    /// upload's block is sealed by the same commit.
    pub fn put_staged(&mut self, synced: &Synced, sealed: bool) {
        let entry = self
            .tiers
            .entry(TIER)
            .or_default()
            .entry(synced.object())
            .or_insert(StagedObject {
                object: synced.object(),
                len: 0,
                sealed,
            });
        entry.len = entry.len.max(synced.end());
    }

    /// `SealStaged`.
    pub fn seal(&mut self, object: ObjectId) {
        self.tiers
            .get_mut(&TIER)
            .unwrap()
            .get_mut(&object)
            .unwrap()
            .sealed = true;
    }

    pub fn staged(&self) -> Vec<StagedObject> {
        self.staged_on(TIER)
    }
}

pub fn options(budget: u64, segment_limit: u64) -> TierOptions {
    TierOptions {
        seal_after: SEAL_AFTER,
        segment_limit,
        ..TierOptions::new(budget)
    }
}

/// Opens `fs` as start `epoch`, numbering new blocks from 0.
pub fn open_tier(
    fs: &FaultFs,
    epoch: u64,
    options: TierOptions,
    index: &Index,
) -> Result<(Tier<FaultFs>, Recovery), TierError> {
    open_tier_as(
        fs,
        kbf_meta::Writer::Epoch(Epoch::new(epoch)),
        options,
        index,
    )
}

/// Opens `fs` as a start of `writer`, numbering new blocks from 0.
pub fn open_tier_as(
    fs: &FaultFs,
    writer: kbf_meta::Writer,
    options: TierOptions,
    index: &Index,
) -> Result<(Tier<FaultFs>, Recovery), TierError> {
    let seq = Arc::new(AtomicU64::new(0));
    let start = TierStart {
        tier: TIER,
        writer,
        options,
    };
    Tier::open(
        fs.clone(),
        start,
        index,
        Box::new(move || seq.fetch_add(1, Ordering::Relaxed)),
    )
}

/// `n` bytes that differ from every other call's: `tag` repeated.
pub fn bytes(tag: u8, n: usize) -> Vec<u8> {
    vec![tag; n]
}

/// Reserves, writes, syncs and commits one grouped range of `data`: an acknowledged
/// write. Returns where it is.
pub fn ack_group(
    tier: &dyn WriteTier,
    index: &mut Index,
    data: &[u8],
    now: Instant,
) -> Result<(ObjectId, u64), TierError> {
    let r = tier.reserve_group(data.len() as u64, now)?;
    tier.write(&r, 0, data)?;
    let synced = tier.sync(r).map_err(|u| u.error)?;
    let at = (synced.object(), synced.offset());
    index.put_staged(&synced, false);
    tier.committed(synced);
    Ok(at)
}

/// The same for a segment upload's block, written in two ranges, the second first.
pub fn ack_upload(
    tier: &dyn WriteTier,
    index: &mut Index,
    data: &[u8],
) -> Result<ObjectId, TierError> {
    let r = tier.reserve_upload(data.len() as u64)?;
    let half = data.len() / 2;
    tier.write(&r, half as u64, &data[half..])?;
    tier.write(&r, 0, &data[..half])?;
    let synced = tier.sync(r).map_err(|u| u.error)?;
    let object = synced.object();
    index.put_staged(&synced, true);
    tier.committed(synced);
    Ok(object)
}

/// The block files in `fs`, sorted.
pub fn blocks(fs: &FaultFs) -> Vec<String> {
    let mut v: Vec<String> = fs
        .list()
        .unwrap()
        .into_iter()
        .filter(|n| n.ends_with(".blk"))
        .collect();
    v.sort();
    v
}

pub fn len_of(fs: &FaultFs, object: ObjectId) -> Option<usize> {
    fs.contents(&block_name(object)).map(|c| c.len())
}
