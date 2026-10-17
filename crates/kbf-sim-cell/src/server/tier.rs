//! The local write tier of the simulated server (docs/design/write-path.md, section 6),
//! run as `kbf-front` runs it (`cache/tier.rs`, `cache/flush.rs`) over `kbf-tier`'s
//! [`Tier`] on the server's second disk:
//!
//! - **Start** ([`Server::attach_tier`]): the log is raised to machine version 2, the
//!   tier is the registered one or a new id (this start's writer epoch), the directory
//!   is opened against the committed staged table ([`Tier::open`]: the `TIER` check,
//!   each staged block cut to its committed length, the orphan sweep), `AddTier`
//!   commits for a new tier, and each lost block is marked `Missing`. What the open
//!   did is recorded, and the directory is checked against the committed state
//!   (invariant E).
//! - **Writes** ([`Server::stage`]): an upload's bytes are written to a range of the
//!   open block, synced, indexed with one `PutStaged`, and only then acknowledged. A
//!   range the tier does not take (its budget, a disk error) goes down the direct path.
//! - **The flusher** ([`Server::flush_round`], every tick): `SealStaged` for each block
//!   the tier names, then up to [`FLUSHES_PER_ROUND`] sealed objects, oldest first:
//!   each re-read and checked, PUT whole (records and the footer `kbf_segments` builds
//!   from the committed list) under its own id, `ObjectFlushed` committed once the
//!   store has it, and only then renamed to a flushed copy. A failed or late PUT, or a
//!   read error, is retried after [`FLUSH_BACKOFF_FIRST`] doubling to
//!   [`FLUSH_BACKOFF_MAX`]; a record that fails its check marks the object `Corrupt`.
//! - **Reads**: each blob a `FindMissingBlobs` answers present is read [`READ_AFTER`]
//!   later, at the location the index gave then, as a client's fetch would: from the
//!   tier's block or flushed copy, else from the store. Between the two the block may
//!   be flushed, renamed and evicted, which is the race of scenario 16.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_front::{FLUSH_BACKOFF_FIRST, FLUSH_BACKOFF_MAX, FLUSHES_PER_ROUND, MetaLog};
use kbf_log::{DecodeError, Format, LogCommand, MachineVersion};
use kbf_meta::{
    Applied, BlobAnswer, Command, Location, ObjectId, Retention, ServerId, StagedPut, StagedState,
    TierId, UnreachableReason,
};
use kbf_raft::Payload;
use kbf_segments::IndexEntry;
use kbf_segments::layout::encode_footer;
use kbf_server::RaftMetaLog;
use kbf_server::raft_log::RUNS;
use kbf_sim::{NodeId, Output};
use kbf_store::{Fault, FaultFs, Fs};
use kbf_tier::{
    Recovery, StagedSnapshot, Tier, TierError, TierOptions, TierStart, WriteTier, block_name,
};
use kbf_types::{Digest, FarmTime};

use super::{PUT_TIMEOUT, Server, T_PUT, T_READ};
use crate::hook::{Down, TierPoint};
use crate::store::Body;
use crate::{Msg, content};

/// How long after a `FindMissingBlobs` answer the server reads each present blob.
pub const READ_AFTER: Duration = Duration::from_secs(2);

/// The one server's id in the tier registry.
const SERVER: ServerId = ServerId::new(1);

/// A server's local write tier: the hot store's budget, its seal-after interval, and
/// the size at which the open block is sealed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierPlan {
    pub budget: u64,
    pub seal_after: Duration,
    pub segment_limit: u64,
}

impl TierPlan {
    fn options(self) -> TierOptions {
        TierOptions {
            seal_after: self.seal_after,
            segment_limit: self.segment_limit,
            ..TierOptions::new(self.budget)
        }
    }
}

/// A crash at a [`TierPoint`]: when, how, the object it was writing or flushing, and
/// what the next start's open did (`None` until it ran).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierCut {
    pub at: FarmTime,
    pub point: TierPoint,
    pub down: Down,
    pub object: Option<ObjectId>,
    pub recovery: Option<Recovery>,
}

/// What the local write tier did, for the checks and tests to read.
#[derive(Clone, Debug, Default)]
pub struct TierSeen {
    /// Each start's tier and what its open found and did.
    pub recoveries: Vec<(FarmTime, TierId, Recovery)>,
    /// Each start's disagreement between the tier directory and the committed state
    /// (invariant E): a file nothing names, a block longer than committed, a budget
    /// that counts bytes nothing holds.
    pub leaks: Vec<String>,
    /// Staged objects whose block a start found missing or short.
    pub lost: Vec<ObjectId>,
    /// Uploads acknowledged through the tier, and the objects they were staged in.
    pub staged_acks: u64,
    pub staged: BTreeSet<ObjectId>,
    /// Uploads the tier did not take at its budget (backpressure), and after a disk
    /// error; both went down the direct path. Of the disk errors, those the disk
    /// answered as full (`ENOSPC`, [`std::io::ErrorKind::StorageFull`]).
    pub full: u64,
    pub disk_errors: u64,
    pub no_space: u64,
    /// `PutStaged` commits refused by the state machine (never expected).
    pub refused: Vec<String>,
    /// `SealStaged` commits, objects flushed, failed flush attempts.
    pub sealed: u64,
    pub flushed: Vec<ObjectId>,
    pub flush_failures: u64,
    /// Flush attempts whose re-read of the block failed (retried, never marked).
    pub reread_failures: u64,
    /// Objects a record of which failed the flusher's check: marked corrupt.
    pub corrupt: Vec<ObjectId>,
    /// Objects a [`TierFlip`](super::ServerFault::TierFlip) damaged, and flips that
    /// found nothing staged.
    pub damaged: BTreeSet<ObjectId>,
    pub flips_missed: u64,
    /// Reads answered with the right bytes; of them, those served by the store after
    /// the local copy the index answer implied (a staged block, or a copy flushed by
    /// this start) was gone; reads of damaged local bytes refused. Of the reads with
    /// the right bytes, those that found the object's flush PUT outstanding (scenario
    /// 17; the commit and the rename after it run in one step here, so no read lands
    /// between them).
    pub reads: u64,
    pub fallbacks: u64,
    pub refused_damaged: u64,
    pub reads_in_flush: u64,
    /// Staged objects a read found a damaged record in, marked `Corrupt` by it, as
    /// `kbf-front`'s `fetch_answer` marks them (the local block is the only copy).
    pub marked_on_read: Vec<ObjectId>,
    /// Reads whose local copy failed with an I/O error a planted fault caused and that
    /// the store could not serve either (the blob was staged): unavailable for now,
    /// not lost.
    pub read_errors: u64,
    /// Reads of a blob the index answered present that found its bytes nowhere, found
    /// other bytes, or whose local read failed with an I/O error no planted fault
    /// caused (invariant A: `kbf-front` would answer an acknowledged blob unavailable).
    pub unread: Vec<String>,
    /// Each crash at a tier point.
    pub cuts: Vec<TierCut>,
    /// The most bytes the tier counted at once, and copies it evicted.
    pub peak: u64,
    pub evicted: u64,
}

/// A read waiting for its time or the store's answer.
#[derive(Clone, Copy, Debug)]
pub(super) struct Read {
    digest: Digest,
    location: Location,
    /// Whether a local copy was there when the index answered: staged, or flushed by
    /// this start (a copy that may be evicted before the read). Of them, staged.
    local: bool,
    staged: bool,
    /// Whether the local copy was gone when it was read.
    missed: bool,
    /// Whether the local read failed with an I/O error a planted disk fault caused:
    /// the read is unavailable, not lost.
    failed: bool,
}

/// The tier of one start.
pub(super) struct CellTier {
    tier: Arc<Tier<FaultFs>>,
    id: TierId,
    /// Failed flushes: failures in a row, and when the next attempt may start.
    retries: BTreeMap<ObjectId, (u32, FarmTime)>,
    /// Flush PUTs waiting for the store, by deadline tag.
    flushing: BTreeMap<u64, ObjectId>,
    /// Reads waiting, by tag.
    reads: BTreeMap<u64, Read>,
}

/// The wait before the next flush attempt after `failures` failures in a row, as the
/// flusher's (`kbf-front`, `flush_backoff`).
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    FLUSH_BACKOFF_FIRST
        .saturating_mul(1 << doublings)
        .min(FLUSH_BACKOFF_MAX)
}

/// The footer entries of `blobs` (sorted by digest) over `records`, or `None` if a
/// record is not all there or is not its blob's bytes.
fn entries(records: &[u8], blobs: &[(Digest, u64)]) -> Option<Vec<IndexEntry>> {
    let mut out = Vec::with_capacity(blobs.len());
    for (digest, offset) in blobs {
        let from = usize::try_from(*offset).ok()?;
        let to = from.checked_add(usize::try_from(digest.size_bytes).ok()?)?;
        let record = records.get(from..to)?;
        if record != content(digest) {
            return None;
        }
        out.push(IndexEntry {
            digest: *digest,
            offset: *offset,
            crc32c: kbf_segments::crc32c(record),
        });
    }
    Some(out)
}

impl Server {
    fn instant(&self, now: FarmTime) -> std::time::Instant {
        self.base + Duration::from_millis(now.as_millis())
    }

    /// The tier and its id, while the server is up with one.
    fn cell_tier(&self) -> Option<(Arc<Tier<FaultFs>>, TierId)> {
        let t = self.up.as_ref()?.tier.as_ref()?;
        Some((Arc::clone(&t.tier), t.id))
    }

    /// The process, in a step that has it up.
    fn up_mut(&mut self) -> &mut super::Up {
        self.up.as_mut().expect("the server is up in this step")
    }

    /// The tier, in a step whose tier call found one attached.
    fn tier_mut(&mut self) -> &mut CellTier {
        let up = self.up_mut();
        up.tier.as_mut().expect("the tier is attached in this step")
    }

    /// The tier's budget use, while it is up.
    #[must_use]
    pub fn tier_usage(&self) -> Option<kbf_tier::Usage> {
        Some(self.cell_tier()?.0.usage())
    }

    /// What a power cut now would leave of the tier's directory.
    #[must_use]
    pub fn tier_durable(&self) -> FaultFs {
        self.tier_disk.crash(0)
    }

    /// The tier reaches `point` with `object`: whether an armed crash fires there.
    fn reach_tier(&mut self, now: FarmTime, point: TierPoint, object: Option<ObjectId>) -> bool {
        let Some((point, down)) = self.tier_hooks.reached(point) else {
            return false;
        };
        self.cut(now, point, down, object);
        self.dying = Some((self.out.len(), down));
        true
    }

    fn cut(&mut self, now: FarmTime, point: TierPoint, down: Down, object: Option<ObjectId>) {
        self.seen.tier.cuts.push(TierCut {
            at: now,
            point,
            down,
            object,
            recovery: None,
        });
    }

    /// A crash armed part-way through the next commit of `kind` (an in-commit point)
    /// is planted in the raft disk at the commit's `after`-th operation.
    fn arm_tier_commit(&mut self, kind: fn(TierPoint) -> bool) -> Option<(TierPoint, Down)> {
        let (point, _, _) = self.tier_hooks.peek().find(|(p, _, _)| kind(*p))?;
        let (point, down) = self.tier_hooks.reached(point)?;
        let after = point.in_commit().unwrap_or_default();
        self.disk.fail_at(self.disk.ops() + after, Fault::Crash);
        self.armed = Some(down);
        Some((point, down))
    }

    /// After a commit with a planted crash: the crash is a cut of this commit if the
    /// commit failed with it.
    fn settle_tier_commit(
        &mut self,
        now: FarmTime,
        planted: Option<(TierPoint, Down)>,
        object: ObjectId,
    ) {
        let failed = self.up.as_ref().is_some_and(|up| up.failed_at.is_some());
        if let (Some((point, down)), true, Some(_)) = (planted, failed, self.armed) {
            self.cut(now, point, down, Some(object));
        }
    }

    /// Attaches the tier at a start, as `Cache::attach_tier` does: `Err(None)` if a
    /// commit failed (the log has stopped), `Err(Some(why))` if the open was refused.
    pub(super) fn attach_tier(&mut self, now: FarmTime) -> Result<(), Option<String>> {
        let Some(plan) = self.plan.tier else {
            return Ok(());
        };
        let up = self.up.as_mut().expect("attached at a start");
        up.log
            .raise_machine_version(MachineVersion::V2)
            .map_err(|_| None)?;
        let current = up.query(|m| m.tier(SERVER));
        let id = current.unwrap_or_else(|| TierId::new(up.writer.get()));
        let snapshot = up.query(StagedSnapshot::of);
        let start = TierStart {
            tier: id,
            writer: kbf_meta::Writer::Epoch(up.writer),
            options: plan.options(),
        };
        let next = Arc::clone(&up.next_object);
        let seq: kbf_tier::NextSeq = Box::new(move || next.fetch_add(1, Ordering::Relaxed));
        let (tier, recovery) = Tier::open(self.tier_disk.clone(), start, &snapshot, seq)
            .map_err(|e| Some(format!("write tier: {e}")))?;
        if current.is_none() {
            let add = Command::AddTier {
                server: SERVER,
                tier: id,
            };
            let added = up.commit(&self.out, add);
            added.filter(|a| *a == Applied::Tier(Ok(()))).ok_or(None)?;
        }
        for object in &recovery.lost {
            let mark = Command::ObjectUnreachable {
                object: *object,
                reason: UnreachableReason::Missing,
            };
            up.commit(&self.out, mark).ok_or(None)?;
        }
        let names: Vec<(String, u64)> = self
            .tier_disk
            .list()
            .unwrap_or_default()
            .into_iter()
            .map(|n| {
                let len = self.tier_disk.file_len(&n).unwrap_or(u64::MAX);
                (n, len)
            })
            .collect();
        let staged = up.query(move |m| {
            let on = m.staged_objects().filter(|(_, s)| s.tier() == id);
            on.map(|(o, s)| (o, s.committed_len()))
                .collect::<BTreeMap<_, _>>()
        });
        let used = tier.usage().used;
        let leaks = crate::check::tier::directory_leaks(&names, &staged, used);
        self.seen.tier.leaks.extend(leaks);
        self.seen.tier.lost.extend(recovery.lost.iter().copied());
        let open = self
            .seen
            .tier
            .cuts
            .iter_mut()
            .filter(|c| c.recovery.is_none());
        open.for_each(|cut| cut.recovery = Some(recovery.clone()));
        self.seen.tier.recoveries.push((now, id, recovery));
        up.tier = Some(CellTier {
            tier: Arc::new(tier),
            id,
            retries: BTreeMap::new(),
            flushing: BTreeMap::new(),
            reads: BTreeMap::new(),
        });
        Ok(())
    }

    /// Records the tier's use after a change.
    fn note_usage(&mut self, usage: kbf_tier::Usage) {
        self.seen.tier.peak = self.seen.tier.peak.max(usage.used);
        self.seen.tier.evicted = self.seen.tier.evicted.max(usage.evicted);
    }

    /// Stages an upload on the tier: whether the tier took it (acknowledged, answered
    /// `UNAVAILABLE` after a failed commit, or the server died on the way). `false`:
    /// no tier, or it did not take the range, and the direct path writes it.
    pub(super) fn stage(&mut self, now: FarmTime, from: &NodeId, digest: Digest) -> bool {
        let Some((tier, id)) = self.cell_tier() else {
            return false;
        };
        let bytes = content(&digest);
        let reserved = tier.reserve_group(bytes.len() as u64, self.instant(now));
        let reservation = match reserved {
            Ok(r) => r,
            Err(TierError::Full { .. }) => {
                self.seen.tier.full += 1;
                return false;
            }
            Err(e) => return self.disk_error(&e),
        };
        self.note_usage(tier.usage());
        let object = reservation.object();
        if let Err(e) = tier.write(&reservation, 0, &bytes) {
            return self.disk_error(&e);
        }
        if self.reach_tier(now, TierPoint::AfterAppend, Some(object)) {
            return true;
        }
        let synced = match tier.sync(reservation) {
            Ok(synced) => synced,
            Err(e) => return self.disk_error(&e.error),
        };
        if self.reach_tier(now, TierPoint::AfterSync, Some(object)) {
            return true;
        }
        let planted = self.arm_tier_commit(|p| matches!(p, TierPoint::InPutStaged { .. }));
        let put = Command::PutStaged {
            tier: id,
            objects: vec![StagedPut {
                object,
                end: synced.end(),
                seal: false,
                blobs: vec![(digest, synced.offset())],
            }],
        };
        let up = self.up.as_mut().expect("staged while up");
        let applied = up.commit(&self.out, put);
        self.settle_tier_commit(now, planted, object);
        let answer = match applied {
            Some(Applied::Staged(Ok(_))) => {
                tier.committed(synced);
                if self.reach_tier(now, TierPoint::BeforeReply, Some(object)) {
                    return true;
                }
                self.seen.acked.push((now, digest, object));
                self.seen.tier.staged_acks += 1;
                self.seen.tier.staged.insert(object);
                Msg::Uploaded { digest }
            }
            other => {
                // `None`: the log stopped. A refusal is recorded (property 9).
                let refused = format!("{other:?}");
                self.seen
                    .tier
                    .refused
                    .extend(other.is_some().then_some(refused));
                Msg::UploadFailed { digest }
            }
        };
        self.out.push(Output::Send {
            to: from.clone(),
            msg: answer,
        });
        self.reach_tier(now, TierPoint::Acked, Some(object));
        true
    }

    /// Records a disk error that sent an upload down the direct path: `false`.
    fn disk_error(&mut self, e: &TierError) -> bool {
        let full = matches!(e, TierError::Io { source, .. } if source.kind() == std::io::ErrorKind::StorageFull);
        self.seen.tier.disk_errors += 1;
        self.seen.tier.no_space += u64::from(full);
        false
    }

    /// One round of the flusher at `now`.
    pub(super) fn flush_round(&mut self, now: FarmTime) {
        let Some((tier, id)) = self.cell_tier() else {
            return;
        };
        for object in tier.poll_seals(self.instant(now)) {
            let up = self.up.as_mut().expect("flushing while up");
            match up.commit(&self.out, Command::SealStaged { object }) {
                Some(Applied::Tier(Ok(()))) => {
                    // Named by `poll_seals` just now, so the tier takes it.
                    let _ = tier.sealed(object);
                    self.seen.tier.sealed += 1;
                }
                _ => return,
            }
        }
        let t = self.tier_mut();
        let mut busy: BTreeSet<ObjectId> = t.flushing.values().copied().collect();
        let waiting = t.retries.iter().filter(|(_, (_, next))| *next > now);
        busy.extend(waiting.map(|(o, _)| *o));
        let due = self.up_mut().query(move |m| {
            let ours = m.staged_objects().filter(|(o, s)| {
                (s.tier(), s.state()) == (id, StagedState::Sealed) && !busy.contains(o)
            });
            let due = ours.take(FLUSHES_PER_ROUND).map(|(o, s)| {
                let blobs: Vec<(Digest, u64)> = s.blobs().map(|(d, at)| (*d, at)).collect();
                (o, s.committed_len(), blobs)
            });
            due.collect::<Vec<_>>()
        });
        for (object, len, blobs) in due {
            self.flush_one(now, &tier, object, len, blobs);
        }
    }

    /// Re-reads and checks `object`'s block and PUTs it whole.
    fn flush_one(
        &mut self,
        now: FarmTime,
        tier: &Tier<FaultFs>,
        object: ObjectId,
        len: u64,
        blobs: Vec<(Digest, u64)>,
    ) {
        let records = match tier.read(object, 0, usize::try_from(len).unwrap_or(usize::MAX)) {
            Ok(records) => records,
            Err(_) => {
                self.seen.tier.reread_failures += 1;
                return self.flush_failed(now, object);
            }
        };
        let Some(entries) = entries(&records, &blobs) else {
            let mark = Command::ObjectUnreachable {
                object,
                reason: UnreachableReason::Corrupt,
            };
            let up = self.up.as_mut().expect("flushing while up");
            // Only once the mark committed: the block is the object's only copy.
            let marked = up.commit(&self.out, mark);
            let marked = marked.filter(|a| *a == Applied::Marked(Ok(())));
            let discarded = marked.map(|_| {
                let _ = tier.discard(object);
                object
            });
            self.seen.tier.corrupt.extend(discarded);
            return;
        };
        let mut bytes = records;
        bytes.extend_from_slice(&encode_footer(len, &entries));
        let tag = T_PUT + self.puts_sent;
        self.puts_sent += 1;
        self.tier_mut().flushing.insert(tag, object);
        let body = Body { blobs, bytes };
        self.out.push(Output::Send {
            to: self.store.clone(),
            msg: Msg::Flush { object, body },
        });
        self.timer(PUT_TIMEOUT, tag);
    }

    /// A flush attempt of `object` failed at `now`: the next waits for the backoff.
    fn flush_failed(&mut self, now: FarmTime, object: ObjectId) {
        self.seen.tier.flush_failures += 1;
        let t = self.tier_mut();
        let failures = t.retries.get(&object).map_or(0, |(n, _)| *n) + 1;
        let next = now.saturating_add(backoff(failures));
        t.retries.insert(object, (failures, next));
    }

    /// A PUT deadline that is a flush's: whether `tag` was one. The flush is failed;
    /// a later answer to it is ignored.
    pub(super) fn flush_deadline(&mut self, now: FarmTime, tag: u64) -> bool {
        let flushing = self.up.as_mut().and_then(|up| up.tier.as_mut());
        let Some(object) = flushing.and_then(|t| t.flushing.remove(&tag)) else {
            return false;
        };
        self.flush_failed(now, object);
        true
    }

    /// The tier's messages: the store's answers to flushes and reads. Any other
    /// message is handed back.
    pub(super) fn tier_path(&mut self, now: FarmTime, msg: Msg) -> Option<Msg> {
        match msg {
            Msg::FlushDone { object } => self.flush_done(now, object),
            Msg::FlushFailed { object } => {
                let t = self.tier_mut();
                let tag = t
                    .flushing
                    .iter()
                    .find(|(_, o)| **o == object)
                    .map(|(t, _)| *t)?;
                t.flushing.remove(&tag);
                self.flush_failed(now, object);
            }
            Msg::Got { tag, held } => self.got(tag, held),
            other => return Some(other),
        }
        None
    }

    /// The store has the flushed `object`: `ObjectFlushed`, then the rename.
    fn flush_done(&mut self, now: FarmTime, object: ObjectId) {
        let (tier, _) = self
            .cell_tier()
            .expect("a store answer reaches a server that is up");
        let t = self.tier_mut();
        let Some(tag) = t
            .flushing
            .iter()
            .find(|(_, o)| **o == object)
            .map(|(t, _)| *t)
        else {
            return; // answered after its deadline, or to an earlier start
        };
        t.flushing.remove(&tag);
        if self.reach_tier(now, TierPoint::AfterFlushPut, Some(object)) {
            return;
        }
        let planted = self.arm_tier_commit(|p| matches!(p, TierPoint::InFlushCommit { .. }));
        let up = self.up.as_mut().expect("flushing while up");
        let applied = up.commit(&self.out, Command::ObjectFlushed { object });
        self.settle_tier_commit(now, planted, object);
        if !matches!(applied, Some(Applied::Tier(Ok(())))) {
            return;
        }
        if self.reach_tier(now, TierPoint::BeforeRename, Some(object)) {
            return;
        }
        // A failed rename leaves the `.blk`, which the next start deletes.
        let _ = tier.flushed(object);
        self.tier_mut().retries.remove(&object);
        self.seen.tier.flushed.push(object);
        self.note_usage(tier.usage());
    }

    /// Starts a read of each of `asked` the index answers present now.
    pub(super) fn start_reads(&mut self, asked: &[Digest]) {
        let Some(up) = self.up.as_ref() else { return };
        if up.tier.is_none() || asked.is_empty() {
            return;
        }
        let asked = asked.to_vec();
        let present = up.query(move |m| {
            let answers = asked.into_iter().map(|d| (d, m.blob(&d)));
            let present = answers.filter_map(|(d, a)| match a {
                BlobAnswer::Present {
                    location,
                    staged_on,
                } => Some((d, location, staged_on)),
                BlobAnswer::Absent | BlobAnswer::Unavailable => None,
            });
            present.collect::<Vec<_>>()
        });
        for (digest, location, staged_on) in present {
            let tag = T_READ + self.reads_sent;
            self.reads_sent += 1;
            let flushed = self.seen.tier.flushed.contains(&location.object);
            let read = Read {
                digest,
                location,
                local: staged_on.is_some() || flushed,
                staged: staged_on.is_some(),
                missed: false,
                failed: false,
            };
            self.tier_mut().reads.insert(tag, read);
            self.timer(READ_AFTER, tag);
        }
    }

    /// A read's time has come: from the tier, else from the store. As `kbf-front`'s
    /// `fetch_answer`: a staged record that fails its digest marks the object
    /// `Corrupt`; a flushed copy that does, or a local read that fails, reads the store.
    pub(super) fn read_due(&mut self, now: FarmTime, tag: u64) {
        // Down, or a read of an earlier start: nothing to read.
        let attached = self.up.as_mut().and_then(|up| up.tier.as_mut());
        let Some((tier, read, in_flush)) = attached.and_then(|t| {
            let read = t.reads.remove(&tag)?;
            let in_flush = t.flushing.values().any(|o| *o == read.location.object);
            Some((Arc::clone(&t.tier), read, in_flush))
        }) else {
            return;
        };
        let Read {
            digest, location, ..
        } = read;
        let len = usize::try_from(digest.size_bytes).unwrap_or(usize::MAX);
        let met = self.tier_disk.faults_met();
        // Whether the store is asked, and whether a planted fault failed the local read.
        let (ask_store, failed) = match tier.read(location.object, location.offset, len) {
            Ok(bytes) if bytes == content(&digest) => {
                self.seen.tier.reads += 1;
                self.seen.tier.reads_in_flush += u64::from(in_flush);
                return;
            }
            Ok(_) => {
                // Damaged bytes, refused; any other bytes break A.
                let damaged = self.seen.tier.damaged.contains(&location.object);
                self.seen.tier.refused_damaged += u64::from(damaged);
                let at = now.as_millis();
                let other = format!(
                    "{digest:?} read other bytes from {} at {at} ms",
                    location.object
                );
                self.seen.tier.unread.extend((!damaged).then_some(other));
                // A staged record is refused; a flushed copy's is read from the store.
                let staged = read.staged;
                let _ = staged.then(|| self.mark_corrupt_on_read(&tier, location.object));
                (!staged, false)
            }
            Err(TierError::Io { .. }) => {
                let injected = self.tier_disk.faults_met() > met;
                let at = now.as_millis();
                let bug = format!(
                    "{digest:?} in {}: the local read failed at {at} ms, and no planted fault failed it",
                    location.object
                );
                self.seen.tier.unread.extend((!injected).then_some(bug));
                (true, injected)
            }
            Err(_) => (true, false),
        };
        if !ask_store {
            return;
        }
        let read = Read {
            missed: true,
            failed,
            ..read
        };
        self.tier_mut().reads.insert(tag, read);
        let msg = Msg::Get {
            object: location.object,
            digest,
            tag,
        };
        self.out.push(Output::Send {
            to: self.store.clone(),
            msg,
        });
    }

    /// A read found a damaged record in staged `object`: its block takes no more
    /// ranges, it is marked `Corrupt` (its block is the only copy), the block is
    /// deleted if sealed, and the read is refused.
    fn mark_corrupt_on_read(&mut self, tier: &Tier<FaultFs>, object: ObjectId) {
        tier.close(object);
        let mark = Command::ObjectUnreachable {
            object,
            reason: UnreachableReason::Corrupt,
        };
        let up = self.up.as_mut().expect("reading while up");
        let marked = up.commit(&self.out, mark);
        let marked = marked.filter(|a| *a == Applied::Marked(Ok(())));
        let marked = marked.map(|_| {
            let _ = tier.discard(object);
            object
        });
        self.seen.tier.marked_on_read.extend(marked);
    }

    /// The store's answer to a read the tier could not serve.
    fn got(&mut self, tag: u64, held: bool) {
        let Some(read) = self.tier_mut().reads.remove(&tag) else {
            return; // asked by an earlier start
        };
        let (digest, object) = (read.digest, read.location.object);
        self.seen.tier.reads += u64::from(held);
        self.seen.tier.fallbacks += u64::from(held & read.missed & read.local & !read.failed);
        self.seen.tier.read_errors += u64::from(!held & read.failed);
        let lost = !held & !read.failed & !self.seen.tier.damaged.contains(&object);
        let unread = format!("{digest:?} in {object} found neither on the tier nor in the store");
        self.seen.tier.unread.extend(lost.then_some(unread));
    }

    /// A bad sector: flips the first byte of the first record of the oldest staged
    /// object on this tier whose block holds committed bytes, if the server is up and
    /// has one.
    pub(super) fn flip_a_staged_record(&mut self) {
        // Down, or no block staged: the fault finds nothing to damage.
        let first = self.up.as_ref().and_then(|up| {
            let id = up.tier.as_ref()?.id;
            up.query(move |m| {
                let mut on = m.staged_objects().filter(|(_, s)| s.tier() == id);
                on.find_map(|(o, s)| s.blobs().map(|(_, at)| at).min().map(|at| (o, at)))
            })
        });
        // A committed record lies within the block's synced bytes.
        let disk = &self.tier_disk;
        let flipped = first.map(|(object, at)| {
            disk.flip(
                &block_name(object),
                usize::try_from(at).expect("a small offset"),
            );
            object
        });
        self.seen.tier.flips_missed += u64::from(flipped.is_none());
        self.seen.tier.damaged.extend(flipped);
    }
}

/// A rollback of the binary at the end of a run (scenario 30): the durable log opened
/// by a binary that runs only machine version 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rollback {
    /// The index of the entry raising the log to machine version 2.
    pub raised_at: Option<u64>,
    /// The index of the first write tier command.
    pub first_tier_command: Option<u64>,
    /// Why the version-1 open was refused (`raft log: entry <index>: ..` when it
    /// stopped at an entry); `None` if it opened.
    pub refused: Option<String>,
    /// Whether this binary still opens the log after that, to the same staged table.
    pub reopened: bool,
}

impl Server {
    /// Opens what a power cut now would leave of the raft directory as a binary that
    /// runs only machine version 1 would, then as this one; `None` while down.
    #[must_use]
    pub fn rollback(&self) -> Option<Rollback> {
        let up = self.up.as_ref()?;
        let durable = self.disk.crash(0);
        let entries = crate::replica::durable_entries(durable.restart()).ok()?;
        let (mut raised_at, mut first_tier_command) = (None, None);
        for entry in &entries {
            let Payload::Command(bytes) = &entry.payload else {
                continue;
            };
            let index = entry.id.index.0;
            match kbf_log::decode(Format::V1, bytes) {
                // The one raise this binary commits: to version 2.
                Ok(LogCommand::MachineVersion(_)) => {
                    raised_at.get_or_insert(index);
                }
                Err(DecodeError::UnknownCommand("MetaCommand")) => {
                    first_tier_command.get_or_insert(index);
                }
                _ => {}
            }
        }
        let _context = self.runtime.enter();
        let open = |runs| {
            super::ready(RaftMetaLog::open_running(
                durable.restart(),
                Retention::default(),
                "cell/",
                super::drawn(up.log.log_id()),
                runs,
            ))
        };
        let refused = open(MachineVersion::V1).err().map(|e| e.to_string());
        let staged = up.query(StagedSnapshot::of);
        let reopened = open(RUNS)
            .ok()
            .is_some_and(|log| super::ready(log.query(StagedSnapshot::of)) == Ok(staged));
        Some(Rollback {
            raised_at,
            first_tier_command,
            refused,
            reopened,
        })
    }
}

#[cfg(test)]
mod tests {
    use kbf_meta::Epoch;
    use kbf_sim::{Event, NodeInput};
    use kbf_types::{DigestFunction, StateMachine};

    use super::super::{ServerPlan, T_START, T_TICK};
    use super::*;

    fn tier_server(seal_after: u64) -> Server {
        let tier = TierPlan {
            budget: 1 << 10,
            seal_after: Duration::from_secs(seal_after),
            segment_limit: 1 << 9,
        };
        let plan = ServerPlan {
            faults: Vec::new(),
            down_for: Duration::from_secs(3),
            tier: Some(tier),
        };
        Server::new(plan, NodeId::from("store"))
    }

    fn input(at: u64, event: Event<Msg>) -> NodeInput<Msg> {
        NodeInput {
            now: FarmTime::from_millis(at),
            entropy: 9,
            event,
        }
    }

    /// What `s` sent since the last call, as text (`Msg` has no `PartialEq`).
    fn sent(s: &mut Server) -> String {
        format!("{:?}", s.take_outputs())
    }

    /// A started tier server that has acknowledged one upload, and the blob.
    fn with_one_upload(seal_after: u64) -> (Server, Digest) {
        let mut s = tier_server(seal_after);
        s.apply(input(0, Event::Start));
        let d = crate::input_of(&crate::action(3));
        let from = NodeId::from("client-0");
        let msg = Msg::Upload { digest: d };
        s.apply(input(10, Event::Message { from, msg }));
        assert!(sent(&mut s).contains("Uploaded"), "{:?}", s.seen.tier);
        (s, d)
    }

    /// Catches a tier open that refuses (a block of a later epoch: a reused epoch)
    /// yet leaves the server up, or goes unrecorded; and a start whose raise or
    /// `AddTier` failed left up (the log has stopped).
    #[test]
    fn a_refused_or_failed_tier_start_takes_the_server_down() {
        let mut s = tier_server(5);
        let later = ObjectId::new(Epoch::new(9), 0);
        s.tier_disk.put(&block_name(later), b"x");
        s.apply(input(0, Event::Start));
        assert!(s.up.is_none());
        let refused = &s.seen.refused;
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert!(refused[0].1.contains("write tier"), "{refused:?}");
        assert!(sent(&mut s).contains(&format!("tag: {T_START}")));
        // The operations of an open and the writer epoch's commit, then the raise's
        // and `AddTier`'s.
        let mut probe = Server::new(ServerPlan::default(), NodeId::from("store"));
        probe.apply(input(0, Event::Start));
        for at in 0..4 {
            let mut s = tier_server(5);
            s.disk.fail_at(probe.disk.ops() + at, Fault::Error);
            s.apply(input(0, Event::Start));
            assert!(s.up.is_none(), "an error at op {at} of the attach");
            assert!(s.seen.refused.is_empty());
        }
    }

    /// Catches a start that does not mark a staged object whose block is gone
    /// `Missing` (its blob would be answered present with no bytes anywhere), or does
    /// not report it.
    #[test]
    fn a_staged_block_gone_at_a_start_is_marked_missing() {
        let (mut s, d) = with_one_upload(5);
        let object = s.seen.acked[0].2;
        s.down(Down::Kill);
        s.tier_disk.remove(&block_name(object)).expect("the block");
        s.apply(input(4_000, Event::Timer { tag: T_START }));
        assert_eq!(s.seen.tier.lost, [object]);
        let meta = s.meta().expect("up");
        assert!(matches!(meta.blob(&d), BlobAnswer::Unavailable));
    }

    /// Catches a flush whose re-read failed that is PUT anyway, never retried, or
    /// retried before its backoff.
    #[test]
    fn a_flush_whose_re_read_failed_is_retried_after_the_backoff() {
        let (mut s, _) = with_one_upload(1);
        // The tick's first operation on the tier's disk is the flusher's re-read.
        s.tier_disk.fail_at(s.tier_disk.ops(), Fault::Error);
        s.apply(input(1_500, Event::Timer { tag: T_TICK }));
        let tier = &s.seen.tier;
        assert_eq!(
            (tier.sealed, tier.flush_failures, tier.reread_failures),
            (1, 1, 1)
        );
        assert!(!sent(&mut s).contains("Flush"));
        s.apply(input(2_000, Event::Timer { tag: T_TICK }));
        assert!(
            !sent(&mut s).contains("Flush"),
            "retried before the backoff"
        );
        s.apply(input(2_600, Event::Timer { tag: T_TICK }));
        assert!(sent(&mut s).contains("Flush"));
    }

    /// Catches a flusher that seals or flushes on a log that has stopped.
    #[test]
    fn a_stopped_log_stops_the_flusher() {
        let (mut s, _) = with_one_upload(1);
        s.disk.fail_at(s.disk.ops(), Fault::Error);
        s.apply(input(1_500, Event::Timer { tag: T_TICK }));
        assert_eq!(s.seen.tier.sealed, 0);
        assert!(!sent(&mut s).contains("Flush"));
    }

    fn digest(n: u8, size: u64) -> Digest {
        Digest::new(DigestFunction::Sha256, [n; 32], size)
    }

    /// Catches a backoff that does not grow, grows without bound, or overflows.
    #[test]
    fn the_flush_backoff_doubles_to_its_cap() {
        assert_eq!(backoff(1), FLUSH_BACKOFF_FIRST);
        assert_eq!(backoff(2), FLUSH_BACKOFF_FIRST * 2);
        assert_eq!(backoff(u32::MAX), FLUSH_BACKOFF_MAX);
    }

    /// Catches a flusher check that passes a record whose bytes differ, or one cut
    /// short by the block's end, or that lists entries out of the committed order.
    #[test]
    fn a_record_must_be_whole_and_its_bytes() {
        let (a, b) = (digest(1, 3), digest(2, 2));
        let mut records = content(&a);
        records.extend(content(&b));
        let blobs = [(a, 0), (b, 3)];
        let listed = entries(&records, &blobs).expect("both whole");
        let order: Vec<(Digest, u64)> = listed.iter().map(|e| (e.digest, e.offset)).collect();
        assert_eq!(order, blobs);
        assert_eq!(listed[1].crc32c, kbf_segments::crc32c(&content(&b)));
        assert!(entries(&records[..4], &blobs).is_none(), "a short record");
        let mut damaged = records.clone();
        damaged[3] ^= 0xFF;
        assert!(entries(&damaged, &blobs).is_none(), "damaged bytes");
    }
}
