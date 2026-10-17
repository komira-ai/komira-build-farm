//! Restart and power cuts over a `FaultFs`: what an acknowledgement survives, what a
//! torn tail becomes, the `TIER` check and the orphan sweep.

mod support;

use std::time::Instant;

use std::num::NonZeroU64;

use kbf_meta::{Epoch, NodeWriter, ObjectId, Writer};
use kbf_store::{FaultFs, Fs, FsFile};
use kbf_tier::{TIER, TierError, TierId, WriteTier, block_name, seg_name};

use support::{
    Index, SEAL_AFTER, TIER as OURS, ack_group, ack_upload, blocks, bytes, len_of, open_tier,
    open_tier_as, options,
};

const LIMIT: u64 = 1 << 20;

fn opts() -> kbf_tier::TierOptions {
    options(1 << 30, LIMIT)
}

/// Catches: an acknowledgement before `fdatasync` (mutant: skip the file sync), so a
/// power cut keeps only the bytes some later sync happened to cover; and a block
/// whose name was not synced before its first acknowledgement (mutant: skip
/// `sync_dir`), so a power cut that keeps no unsynced name change loses the file.
#[test]
fn acknowledged_ranges_read_back_after_a_power_cut() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let (tier, _) = open_tier(&fs, 1, opts(), &index).unwrap();
    let a = ack_group(&tier, &mut index, &bytes(1, 10), now).unwrap();
    let b = ack_group(&tier, &mut index, &bytes(2, 7), now).unwrap();
    let u = ack_upload(&tier, &mut index, &bytes(3, 9)).unwrap();
    assert_eq!(a.0, b.0);
    let cut = fs.crash_reordered(0, 0);
    let (tier, rec) = open_tier(&cut, 2, opts(), &index).unwrap();
    assert_eq!(rec.lost, [], "{rec:?}");
    assert_eq!(tier.read(a.0, a.1, 10).unwrap(), bytes(1, 10));
    assert_eq!(tier.read(b.0, b.1, 7).unwrap(), bytes(2, 7));
    assert_eq!(tier.read(u, 0, 9).unwrap(), bytes(3, 9));
}

/// Catches: bytes past the committed length (synced, never named by a committed
/// `PutStaged`) kept at a restart (mutant: no truncation), where a later flush or
/// read would treat them as stored.
#[test]
fn bytes_past_the_committed_length_are_cut_at_restart() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let (tier, _) = open_tier(&fs, 1, opts(), &index).unwrap();
    let (object, _) = ack_group(&tier, &mut index, b"kept", now).unwrap();
    let r = tier.reserve_group(4, now).unwrap();
    tier.write(&r, 0, b"torn").unwrap();
    let synced = tier.sync(r).unwrap();
    std::mem::forget(synced);
    let cut = fs.crash(0);
    assert_eq!(len_of(&cut, object), Some(8));
    let (tier, rec) = open_tier(&cut, 2, opts(), &index).unwrap();
    assert_eq!(rec.truncated, [object]);
    assert_eq!(len_of(&cut, object), Some(4));
    assert_eq!(cut.crash(0).contents(&block_name(object)).unwrap(), b"kept");
    assert!(matches!(
        tier.read(object, 0, 8),
        Err(TierError::NotCommitted { committed: 4, .. })
    ));
}

/// Catches: committed lengths taken from the last commit rather than the largest
/// across a power cut: two ranges of one block committed out of order are both read
/// back.
#[test]
fn out_of_order_commits_survive_a_power_cut() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let (tier, _) = open_tier(&fs, 1, opts(), &index).unwrap();
    let first = tier.reserve_group(3, now).unwrap();
    let second = tier.reserve_group(3, now).unwrap();
    tier.write(&second, 0, b"two").unwrap();
    tier.write(&first, 0, b"one").unwrap();
    let second = tier.sync(second).unwrap();
    index.put_staged(&second, false);
    tier.committed(second);
    let first = tier.sync(first).unwrap();
    let object = first.object();
    index.put_staged(&first, false);
    tier.committed(first);
    let (tier, rec) = open_tier(&fs.crash(0), 2, opts(), &index).unwrap();
    assert_eq!(rec.unsealed, [object]);
    assert_eq!(tier.read(object, 0, 6).unwrap(), b"onetwo");
}

/// Catches: orphan blocks left on disk, filling it (an upload open at the restart, an
/// upload synced before its `PutStaged` committed, a grouped block nothing committed
/// in), a sweep whose deletions are not made durable (mutant: no `sync_dir` after
/// it), and a sweep that deletes a flushed `.seg` copy or a file the tier does not
/// own.
#[test]
fn the_orphan_sweep_deletes_every_unindexed_block_durably() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let (tier, _) = open_tier(&fs, 1, opts(), &index).unwrap();
    let kept = ack_upload(&tier, &mut index, b"kept").unwrap();
    let open = tier.reserve_upload(4).unwrap();
    tier.write(&open, 0, b"open").unwrap();
    let synced = tier.reserve_upload(4).unwrap();
    tier.write(&synced, 0, b"sync").unwrap();
    std::mem::forget(tier.sync(synced).unwrap());
    let grouped = tier.reserve_group(4, now).unwrap();
    tier.write(&grouped, 0, b"grup").unwrap();
    std::mem::forget(tier.sync(grouped).unwrap());
    std::mem::forget(open);
    for name in ["0000000000000000-0000000000000009.seg", "notes", "TIER.tmp"] {
        fs.put(name, b"x");
    }
    let fs = fs.restart();
    assert_eq!(blocks(&fs).len(), 4);
    let (tier, rec) = open_tier(&fs, 2, opts(), &index).unwrap();
    assert_eq!(rec.removed.len(), 3, "{rec:?}");
    assert_eq!(blocks(&fs), [block_name(kept)]);
    assert_eq!(blocks(&fs.crash_reordered(0, 0)), [block_name(kept)]);
    assert_eq!(tier.usage().used, 4, "orphans consume no budget");
    let mut left = fs.list().unwrap();
    left.sort();
    assert_eq!(
        left,
        [
            "0000000000000000-0000000000000009.seg".to_owned(),
            block_name(kept),
            TIER.to_owned(),
            "TIER.tmp".to_owned(),
            "notes".to_owned()
        ],
        "TIER matched, so its temporary file is not touched"
    );
}

/// Catches: a start that proceeds over a missing, damaged or wrong `TIER` while
/// objects are staged on this tier, or in a directory of another tier with staged
/// objects (a wrong mount or flag would delete every acknowledged block as an
/// orphan); and a first start whose `TIER` is not durable.
#[test]
fn a_wrong_or_missing_tier_file_refuses_the_start() {
    let fs = FaultFs::new();
    let (_tier, _) = open_tier(&fs, 1, opts(), &Index::default()).unwrap();
    assert_eq!(
        fs.crash_reordered(0, 0).contents(TIER).unwrap(),
        b"1\n",
        "TIER is durable once open returns"
    );

    let mut index = Index::default();
    let (tier, _) = open_tier(&fs, 2, opts(), &index).unwrap();
    let object = ack_upload(&tier, &mut index, b"data").unwrap();
    drop(tier);
    for (content, found) in [
        (None, None),
        (Some(&b"9\n"[..]), Some(TierId::new(9))),
        (Some(b"junk"), None),
    ] {
        let fs = fs.restart();
        fs.remove(TIER).unwrap();
        if let Some(bytes) = content {
            fs.put(TIER, bytes);
        }
        let err = open_tier(&fs, 3, opts(), &index).unwrap_err();
        assert!(
            matches!(err, TierError::WrongTier { expected: OURS, found: f } if f == found),
            "{err:?}"
        );
        assert_eq!(blocks(&fs), [block_name(object)], "nothing deleted");
    }

    let mut theirs = Index::default();
    theirs
        .tiers
        .insert(TierId::new(9), index.tiers[&OURS].clone());
    let other = fs.restart();
    other.remove(TIER).unwrap();
    other.put(TIER, b"9\n");
    assert!(matches!(
        open_tier(&other, 3, opts(), &theirs),
        Err(TierError::OtherTier { found }) if found == TierId::new(9)
    ));
    assert_eq!(blocks(&other).len(), 1);

    let abandoned = fs.restart();
    abandoned.remove(TIER).unwrap();
    abandoned.put(TIER, b"9\n");
    let (_tier, rec) = open_tier(&abandoned, 3, opts(), &Index::default()).unwrap();
    assert_eq!(
        rec.removed,
        [block_name(object)],
        "nothing staged anywhere: orphans"
    );
    assert_eq!(abandoned.contents(TIER).unwrap(), b"1\n");
}

/// Catches: one missing or short block failing the whole start (or the whole tier),
/// rather than only its own object; and a short block served, even one only a
/// single byte short of its committed length.
#[test]
fn one_missing_or_short_block_loses_only_its_object() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let (tier, _) = open_tier(&fs, 1, opts(), &index).unwrap();
    let a = ack_upload(&tier, &mut index, b"aaaa").unwrap();
    let b = ack_upload(&tier, &mut index, b"bbbb").unwrap();
    let c = ack_upload(&tier, &mut index, b"cccc").unwrap();
    drop(tier);
    let fs = fs.restart();
    fs.remove(&block_name(a)).unwrap();
    fs.put(&block_name(b), b"bbb");
    let (tier, rec) = open_tier(&fs, 2, opts(), &index).unwrap();
    assert_eq!(rec.lost, [a, b]);
    assert_eq!(rec.sealed, [c]);
    assert_eq!(tier.read(c, 0, 4).unwrap(), b"cccc");
    assert!(matches!(
        tier.read(b, 0, 2),
        Err(TierError::UnknownBlock(_))
    ));
    assert_eq!(tier.usage().used, 4);
}

/// Catches: a start whose epoch is not new (a block of this epoch or a later one is
/// on disk) appending to or deleting another start's block.
#[test]
fn a_block_of_this_epoch_refuses_the_start() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let (tier, _) = open_tier(&fs, 5, opts(), &index).unwrap();
    let object = ack_upload(&tier, &mut index, b"live").unwrap();
    drop(tier);
    for epoch in [5, 4] {
        assert!(matches!(
            open_tier(&fs, epoch, opts(), &Index::default()),
            Err(TierError::FutureBlock { name }) if name == block_name(object)
        ));
    }
    assert_eq!(blocks(&fs), [block_name(object)]);
}

fn node(member: u64, boot: u64) -> Writer {
    let n = |v| NonZeroU64::new(v).expect("not zero");
    Writer::Node(NodeWriter::new(n(member), n(boot)))
}

/// Catches: a node writer's start that takes a block of its own boot or a later one
/// (the boot was reused: it would append to, or delete, another process's block), and
/// one that refuses over an earlier boot's staged block or another member's (it must
/// recover the first and leave the second as an orphan, as an earlier epoch's). The
/// new block is named by the node writer, so a third start sees it. Mutant seen red:
/// `at_or_after` comparing members only (the earlier boot's block refuses the start).
#[test]
fn a_node_writer_restarts_over_earlier_boots_and_refuses_its_own() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let (tier, _) = open_tier_as(&fs, node(7, 2), opts(), &index).unwrap();
    let object = ack_upload(&tier, &mut index, b"live").unwrap();
    assert_eq!(object.writer(), node(7, 2));
    assert_eq!(
        block_name(object),
        "0000000000000007-0000000000000002-0000000000000000.blk"
    );
    drop(tier);
    for again in [node(7, 2), node(7, 1)] {
        assert!(matches!(
            open_tier_as(&fs, again, opts(), &Index::default()),
            Err(TierError::FutureBlock { name }) if name == block_name(object)
        ));
    }
    let (tier, rec) = open_tier_as(&fs, node(7, 3), opts(), &index).unwrap();
    assert_eq!(rec.sealed, [object]);
    assert_eq!(tier.read(object, 0, 4).unwrap(), b"live");
    drop(tier);
    let (_, rec) = open_tier_as(&fs, node(8, 1), opts(), &Index::default()).unwrap();
    assert_eq!(rec.removed, [block_name(object)]);
}

/// Catches: an unsealed staged block left open after a restart (it would never be
/// sealed or flushed), its committed bytes not counted against the budget, and new
/// ranges appended to a block of an earlier start.
#[test]
fn a_restart_seals_what_was_open_and_counts_what_is_staged() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let (tier, _) = open_tier(&fs, 1, opts(), &index).unwrap();
    let (open, _) = ack_group(&tier, &mut index, b"open", now).unwrap();
    let sealed = ack_upload(&tier, &mut index, b"sealed").unwrap();
    drop(tier);
    let (tier, rec) = open_tier(&fs.crash(0), 2, opts(), &index).unwrap();
    assert_eq!((rec.unsealed, rec.sealed), (vec![open], vec![sealed]));
    assert_eq!(tier.usage().used, 10);
    assert_eq!(tier.poll_seals(now), [open]);
    tier.sealed(open).unwrap();
    index.seal(open);
    assert!(tier.poll_seals(now + SEAL_AFTER).is_empty());
    let r = tier.reserve_group(1, now).unwrap();
    assert_eq!(r.object().epoch().map(Epoch::get), Some(2));
    assert_eq!(r.offset(), 0);
    let staged: Vec<ObjectId> = index.staged().iter().map(|s| s.object).collect();
    assert_eq!(staged, [open, sealed]);
}

/// Every operation `open` runs on `fs`, failed one at a time: each failure is
/// reported as an I/O error, never a start over a half-checked directory.
fn every_failure_of_open_is_reported(fs: &FaultFs, index: &Index) {
    let ops_of_open = {
        let probe = fs.restart();
        let start = probe.ops();
        open_tier(&probe, 9, opts(), index).unwrap();
        probe.ops() - start
    };
    assert!(ops_of_open > 3, "{ops_of_open}");
    for op in 0..ops_of_open {
        let attempt = fs.restart();
        attempt.fail_at(attempt.ops() + op, kbf_store::Fault::Error);
        let result = open_tier(&attempt, 9, opts(), index);
        assert!(matches!(result, Err(TierError::Io { .. })), "op {op}");
    }
}

/// Catches: open's own disk steps unchecked (listing, reading `TIER`, the length,
/// truncation and sync of a torn block, the `TIER` write, an orphan's removal and the
/// directory sync after it), and the temporary `TIER` a crash during an earlier first
/// start left not cleared (the create would then fail at every start).
#[test]
fn open_reports_its_own_disk_failures_and_clears_a_stale_tier_tmp() {
    let fresh = FaultFs::new();
    fresh.put("TIER.tmp", b"half");
    every_failure_of_open_is_reported(&fresh, &Index::default());
    let (tier, _) = open_tier(&fresh, 1, opts(), &Index::default()).unwrap();
    assert_eq!(fresh.contents(TIER).unwrap(), b"1\n");
    assert_eq!(fresh.contents("TIER.tmp"), None);

    let mut index = Index::default();
    let object = ack_upload(&tier, &mut index, b"ab").unwrap();
    let orphan = tier.reserve_upload(1).unwrap();
    std::mem::forget(orphan);
    drop(tier);
    let fs = fresh.restart();
    fs.open_append(&block_name(object))
        .unwrap()
        .append(b"tail")
        .unwrap();
    fs.put(
        &kbf_tier::seg_name(ObjectId::new(kbf_meta::Epoch::new(1), 99)),
        b"copy",
    );
    every_failure_of_open_is_reported(&fs, &index);
}

/// Catches: a clear whose listing, removal or directory sync fails reported as done
/// (the start would then reuse names an earlier start's files still hold).
#[test]
fn every_failure_of_clear_is_reported() {
    let fs = FaultFs::new();
    fs.put(TIER, b"1\n");
    fs.put(
        &block_name(ObjectId::new(kbf_meta::Epoch::new(1), 1)),
        b"block",
    );
    let ops = {
        let probe = fs.restart();
        kbf_tier::clear(&probe).unwrap();
        probe.ops()
    };
    assert_eq!(ops, 4, "list, two removals, the directory sync");
    for op in 0..ops {
        let attempt = fs.restart();
        attempt.fail_at(op, kbf_store::Fault::Error);
        let result = kbf_tier::clear(&attempt);
        assert!(
            matches!(result, Err(TierError::Io { .. })),
            "op {op}: {result:?}"
        );
    }
}

/// Catches: a clear (a start over an empty metadata log) that leaves a block, a
/// flushed copy or `TIER` behind (the new start's objects would reuse their names),
/// deletes a file the tier does not write, or does not sync the deletions.
#[test]
fn clear_deletes_every_file_the_tier_writes_and_nothing_else() {
    let fs = FaultFs::new();
    let index = Index::default();
    let (tier, _) = open_tier(&fs, 1, opts(), &index).unwrap();
    let mut index = index;
    let object = ack_upload(&tier, &mut index, &bytes(1, 4)).unwrap();
    drop(tier);
    fs.put("0000000000000001-0000000000000009.seg", b"copy");
    fs.put("TIER.tmp", b"7\n");
    fs.put("notes.txt", b"not ours");
    let mut cleared = kbf_tier::clear(&fs).unwrap();
    cleared.sort();
    let mut want = vec![
        "0000000000000001-0000000000000009.seg".to_owned(),
        TIER.to_owned(),
        "TIER.tmp".to_owned(),
        block_name(object),
    ];
    want.sort();
    assert_eq!(cleared, want);
    let cut = fs.crash_reordered(0, 0);
    assert_eq!(cut.list().unwrap(), ["notes.txt"]);
    assert_eq!(kbf_tier::clear(&cut).unwrap(), Vec::<String>::new());
}

/// Catches a start without flushed copies keeping the copies an earlier run kept (a
/// node switched to one cache would hold them, uncounted by the one cache, forever),
/// counting them, or deleting a staged block with them: the copy is deleted and listed
/// with the removed files; the staged block stays, readable and counted.
#[test]
fn a_start_without_flushed_copies_deletes_the_earlier_runs_copies() {
    let fs = FaultFs::new();
    let mut index = Index::default();
    let now = Instant::now();
    let (tier, _) = open_tier(&fs, 1, options(1 << 20, LIMIT), &index).unwrap();
    let (flushed, _) = ack_group(&tier, &mut index, &bytes(1, 10), now).unwrap();
    for object in tier.poll_seals(now + SEAL_AFTER) {
        index.seal(object);
        tier.sealed(object).unwrap();
    }
    tier.flushed(flushed).unwrap();
    index.tiers.get_mut(&OURS).unwrap().remove(&flushed);
    let staged = ack_upload(&tier, &mut index, &bytes(2, 6)).unwrap();
    drop(tier);
    assert!(fs.contents(&seg_name(flushed)).is_some());
    let fs = fs.restart();
    let options = options(1 << 20, LIMIT).without_flushed_copies();
    let (tier, recovery) = open_tier(&fs, 2, options, &index).unwrap();
    assert_eq!(fs.contents(&seg_name(flushed)), None);
    assert_eq!(recovery.removed, [seg_name(flushed)]);
    assert!(recovery.flushed.is_empty(), "{recovery:?}");
    assert_eq!(tier.read(staged, 0, 6).unwrap(), bytes(2, 6));
    let usage = tier.usage();
    assert_eq!((usage.used, usage.flushed), (6, 0));
    drop(tier);
    // A copy that cannot be deleted (or, kept, measured) stops the start, whichever
    // step fails.
    fs.put(&seg_name(flushed), &bytes(1, 10));
    for options in [options, opts()] {
        let ops = {
            let probe = fs.restart();
            let start = probe.ops();
            open_tier(&probe, 3, options, &index).unwrap();
            probe.ops() - start
        };
        for op in 0..ops {
            let attempt = fs.restart();
            attempt.fail_at(attempt.ops() + op, kbf_store::Fault::Error);
            let opened = open_tier(&attempt, 3, options, &index);
            assert!(matches!(opened, Err(TierError::Io { .. })), "op {op}");
        }
    }
    let (_, recovery) = open_tier(&fs.restart(), 3, opts(), &index).unwrap();
    assert_eq!(recovery.flushed, [flushed], "a kept copy is listed");
}
