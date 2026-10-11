//! The log, the hard state and the newest snapshot in one directory.

use std::io;

use kbf_raft::{Entry, HardState, Snapshot, Storage, Stored};
use thiserror::Error;

use crate::fs::{Fs, FsFile};
use crate::record::{
    MAX_PAYLOAD, any_record, decode_entry, decode_hard, decode_snapshot, encode_entry, encode_hard,
    encode_snapshot, get_record, put_record,
};

/// The file that holds the hard state.
pub const HARD_STATE: &str = "hardstate";

/// The file a new hard state is written to before it is renamed over [`HARD_STATE`].
pub const HARD_STATE_TMP: &str = "hardstate.tmp";

/// The file that holds the newest snapshot.
pub const SNAPSHOT: &str = "snapshot";

/// The file a new snapshot is written to before it is renamed over [`SNAPSHOT`].
pub const SNAPSHOT_TMP: &str = "snapshot.tmp";

/// How a [`Store`] lays out its log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Appends go to a new segment once the current one holds at least this many
    /// bytes, so a segment can exceed it by one batch.
    pub segment_bytes: u64,
}

impl Default for Options {
    /// 64 MiB segments.
    fn default() -> Self {
        Self {
            segment_bytes: 64 << 20,
        }
    }
}

/// Why the store refused or stopped.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A filesystem call failed. If it was a write or a sync, the store has stopped:
    /// every later call returns [`StoreError::Failed`].
    ///
    /// The stop holds only inside this process. A reopen after a failed sync, before
    /// the machine restarts, can read back bytes that are not on the disk: on Linux a
    /// failed fsync can leave the unwritten pages in the page cache marked clean, so
    /// [`Store::open`] reads them and its own sync returns `Ok`. The store has no way
    /// to tell those bytes from durable ones.
    #[error("storage I/O failed: {0}")]
    Io(#[from] io::Error),
    /// An earlier write or sync failed; the store writes nothing more.
    #[error("the store stopped after an earlier I/O error")]
    Failed,
    /// What is on the disk is damaged somewhere other than a torn tail, or does not
    /// form one log. The store refuses to open; nothing is repaired or skipped.
    #[error("{file} at byte {offset}: {reason}")]
    Corrupt {
        /// The damaged file.
        file: String,
        /// Where in it.
        offset: u64,
        /// What is wrong.
        reason: &'static str,
    },
    /// The caller's entries do not continue the log; nothing was written.
    #[error("invalid entries: {0}")]
    Invalid(&'static str),
}

/// What [`Store::open`] read back: the last hard state persisted (the default if
/// none ever was), the newest snapshot, and the log after the snapshot's base (from
/// index 1 with no snapshot). It is what [`Storage::load`] returns.
pub type Recovered = Stored;

#[derive(Debug)]
struct Segment {
    seq: u64,
    first: u64,
    name: String,
}

fn segment_name(seq: u64, first: u64) -> String {
    format!("{seq:020}-{first:020}.log")
}

fn parse_segment_name(name: &str) -> Option<(u64, u64)> {
    let stem = name.strip_suffix(".log")?;
    let (seq, first) = stem.split_once('-')?;
    let ok = |s: &str| s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit());
    if !ok(seq) || !ok(first) {
        return None;
    }
    Some((seq.parse().ok()?, first.parse().ok()?))
}

fn corrupt(file: &str, offset: usize, reason: &'static str) -> StoreError {
    StoreError::Corrupt {
        file: file.to_owned(),
        offset: offset as u64,
        reason,
    }
}

/// A Raft server's durable state: a segmented, append-only log and the hard state.
///
/// Every successful persist is durable when it returns. The first write or sync
/// error stops the store for good ([`StoreError::Failed`] from then on): after a
/// failed sync what reached the disk is unknown, and a retried sync can report success
/// for bytes that were dropped, so the store never acknowledges anything again. A
/// new process that reopens the store before the machine restarts is not covered by
/// this (see [`StoreError::Io`]).
#[derive(Debug)]
pub struct Store<F: Fs> {
    fs: F,
    options: Options,
    /// The segments still holding live entries, in sequence order.
    segments: Vec<Segment>,
    /// The last segment, open for appending, and its length in bytes.
    active: Option<(F::File, u64)>,
    next_seq: u64,
    /// The index of the last entry: of the log, or the snapshot's base if the log
    /// ends at or below it.
    last: u64,
    /// The snapshot's base index; 0 with no snapshot.
    base: u64,
    failed: bool,
}

impl<F: Fs> Store<F> {
    /// Opens the store in `fs` and reads back what it holds.
    ///
    /// The segments are read in sequence order, skipping any that a later segment
    /// replaces whole (its first index is at or below theirs); a segment whose first
    /// index is at or below the log's last index replaces the entries from that index
    /// on (that is how a truncation was written). A record that is not whole or fails
    /// its CRC is a torn tail when it is in the last segment and no whole record
    /// follows it anywhere in that file: the file is cut back to the last whole record.
    /// Anywhere else, the store refuses to open ([`StoreError::Corrupt`]).
    ///
    /// With a [`SNAPSHOT`] file, the log starts after the snapshot's base: entries at
    /// or below it are read and checked but not returned. The segments that hold only
    /// such entries are skipped: every one before the last segment starting at or
    /// below the base's index + 1 is not read at all, and the last segment, if the log
    /// ends at or below the base, is read and then dropped. A snapshot file is renamed
    /// into place only once it is synced, so any damage in it refuses the open.
    ///
    /// Before returning, the last segment and the directory are synced, so what open
    /// returns is durable even if the process that wrote it died before its own
    /// sync. That holds only if no sync on this directory has failed since the
    /// machine started (see [`StoreError::Io`]). Then the skipped segments are
    /// removed, and a leftover [`HARD_STATE_TMP`] and [`SNAPSHOT_TMP`] are removed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] for damage other than a torn tail (including a log
    /// that does not start at index 1, or right after the snapshot's base, or has a
    /// gap between segments), and [`StoreError::Io`] if a filesystem call fails.
    pub fn open(fs: F, options: Options) -> Result<(Self, Recovered), StoreError> {
        let scan = scan(&fs)?;
        let mut active = None;
        if let Some(seg) = scan.segments.last() {
            let mut file = fs.open_append(&seg.name)?;
            if let Some(at) = scan.torn_at {
                file.truncate(at)?;
            }
            file.sync()?;
            active = Some((file, scan.active_len));
        }
        fs.sync_dir()?;
        for seg in &scan.dead {
            fs.remove(&seg.name)?;
        }
        for tmp in &scan.leftovers {
            fs.remove(tmp)?;
        }

        let store = Self {
            fs,
            options,
            segments: scan.segments,
            active,
            next_seq: scan.next_seq,
            last: scan.last,
            base: scan.stored.base().index.0,
            failed: false,
        };
        Ok((store, scan.stored))
    }

    /// The index of the last entry in the log, or the snapshot's base index if no
    /// entry follows it; 0 when both are empty.
    #[must_use]
    pub const fn last_index(&self) -> u64 {
        self.last
    }

    /// Makes `hard` durable: written to [`HARD_STATE_TMP`], synced, renamed over
    /// [`HARD_STATE`], and the directory synced.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] if a filesystem call fails (the store then stops), and
    /// [`StoreError::Failed`] if it had stopped before.
    pub fn persist_hard_state(&mut self, hard: HardState) -> Result<(), StoreError> {
        let mut bytes = Vec::new();
        put_record(&mut bytes, &encode_hard(hard));
        self.guard(|s| {
            let mut file = s.fs.create(HARD_STATE_TMP)?;
            file.append(&bytes)?;
            file.sync()?;
            s.fs.rename(HARD_STATE_TMP, HARD_STATE)?;
            s.fs.sync_dir()
        })
    }

    /// Makes `entries` durable as the end of the log: every entry at or after the
    /// first one's index is dropped, then `entries` are appended.
    ///
    /// An append that continues the log goes to the last segment, then a sync; once
    /// that segment holds [`Options::segment_bytes`], to a new one. An append that
    /// drops entries never rewrites a file: it goes to a new segment, which open reads
    /// as replacing the entries from its first index on. A new segment is synced and
    /// then the directory, before this returns; the segments it replaced whole are then
    /// removed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Failed`] if the store had stopped before;
    /// [`StoreError::Invalid`] (nothing written, the store keeps running) if `entries`
    /// is empty, not numbered consecutively, starts at or below the snapshot's base
    /// (at 0 with no snapshot) or past the index after the log's last, or holds an
    /// entry whose record would exceed 64 MiB;
    /// [`StoreError::Io`] if a filesystem call fails (the store then stops).
    pub fn persist_entries(&mut self, entries: &[Entry]) -> Result<(), StoreError> {
        if self.failed {
            return Err(StoreError::Failed);
        }
        let first = entries
            .first()
            .ok_or(StoreError::Invalid("no entries"))?
            .id
            .index
            .0;
        if first <= self.base || first > self.last + 1 {
            return Err(StoreError::Invalid("entries do not continue the log"));
        }
        let mut bytes = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            if e.id.index.0 != first + i as u64 {
                return Err(StoreError::Invalid("entries not numbered consecutively"));
            }
            let payload = encode_entry(e);
            if payload.len() > MAX_PAYLOAD {
                return Err(StoreError::Invalid("entry over 64 MiB"));
            }
            put_record(&mut bytes, &payload);
        }
        let last = first + entries.len() as u64 - 1;
        self.guard(|s| {
            let fits = s
                .active
                .as_ref()
                .is_some_and(|(_, len)| *len < s.options.segment_bytes);
            if let Some((file, len)) = s.active.as_mut().filter(|_| fits && first == s.last + 1) {
                file.append(&bytes)?;
                file.sync()?;
                *len += bytes.len() as u64;
            } else {
                let seq = s.next_seq;
                let name = segment_name(seq, first);
                let mut file = s.fs.create(&name)?;
                s.next_seq += 1;
                file.append(&bytes)?;
                file.sync()?;
                s.fs.sync_dir()?;
                s.active = Some((file, bytes.len() as u64));
                let (replaced, kept) = std::mem::take(&mut s.segments)
                    .into_iter()
                    .partition(|seg| seg.first >= first);
                s.segments = kept;
                s.segments.push(Segment { seq, first, name });
                for seg in replaced {
                    s.fs.remove(&seg.name)?;
                }
            }
            s.last = last;
            Ok(())
        })
    }

    /// Makes `snapshot` the newest snapshot, then drops the log through its base.
    ///
    /// The snapshot is written to [`SNAPSHOT_TMP`], synced, renamed over [`SNAPSHOT`],
    /// and the directory synced. Only then are the segments that hold no entry after
    /// the base removed (those removals are not synced: open skips such a segment if
    /// it comes back). A segment that holds the base and entries after it is kept
    /// whole; its entries through the base are hidden by the base the snapshot file
    /// records. The base may lie past the log's last entry; the log is then empty
    /// and the next entry is the one after the base.
    ///
    /// # Errors
    ///
    /// [`StoreError::Failed`] if the store had stopped before;
    /// [`StoreError::Invalid`] (nothing written, the store keeps running) if the base
    /// is not past the current snapshot's (or is index 0);
    /// [`StoreError::Io`] if a filesystem call fails (the store then stops).
    pub fn persist_snapshot(&mut self, snapshot: &Snapshot) -> Result<(), StoreError> {
        if self.failed {
            return Err(StoreError::Failed);
        }
        let base = snapshot.base.index.0;
        if base <= self.base {
            return Err(StoreError::Invalid("snapshot not past the current base"));
        }
        let bytes = encode_snapshot(snapshot);
        self.guard(|s| {
            let mut file = s.fs.create(SNAPSHOT_TMP)?;
            file.append(&bytes)?;
            file.sync()?;
            s.fs.rename(SNAPSHOT_TMP, SNAPSHOT)?;
            s.fs.sync_dir()?;
            // The snapshot is durable: only now may what it covers go.
            let keep_from = if s.last <= base {
                s.active = None;
                s.segments.len()
            } else {
                s.segments
                    .iter()
                    .rposition(|seg| seg.first <= base + 1)
                    .unwrap_or(0)
            };
            s.base = base;
            s.last = s.last.max(base);
            for seg in s.segments.drain(..keep_from) {
                s.fs.remove(&seg.name)?;
            }
            Ok(())
        })
    }

    /// Runs `write`, stopping the store for good if it fails.
    fn guard(&mut self, write: impl FnOnce(&mut Self) -> io::Result<()>) -> Result<(), StoreError> {
        if self.failed {
            return Err(StoreError::Failed);
        }
        write(self).map_err(|e| {
            self.failed = true;
            StoreError::Io(e)
        })
    }
}

/// The kbf-raft host's storage. Every write is durable when it returns, so a sync has
/// nothing left to do.
impl<F: Fs> Storage for Store<F> {
    type Error = StoreError;

    /// Reads the directory again, as [`Store::open`] does but repairing nothing.
    fn load(&mut self) -> Result<Stored, StoreError> {
        if self.failed {
            return Err(StoreError::Failed);
        }
        Ok(scan(&self.fs)?.stored)
    }

    fn write_hard_state(&mut self, hard: HardState) -> Result<(), StoreError> {
        self.persist_hard_state(hard)
    }

    fn write_entries(&mut self, entries: &[Entry]) -> Result<(), StoreError> {
        self.persist_entries(entries)
    }

    /// `Ok` unless the store has stopped.
    fn sync(&mut self) -> Result<(), StoreError> {
        if self.failed {
            return Err(StoreError::Failed);
        }
        Ok(())
    }

    fn write_snapshot(&mut self, snapshot: &Snapshot) -> Result<(), StoreError> {
        self.persist_snapshot(snapshot)
    }
}

/// What the directory holds, and what [`Store::open`] must repair.
struct Scan {
    stored: Stored,
    /// The segments holding the log after the snapshot's base, in sequence order.
    segments: Vec<Segment>,
    /// Segments a later one replaces whole or the snapshot covers.
    dead: Vec<Segment>,
    /// Leftover temporary files.
    leftovers: Vec<&'static str>,
    next_seq: u64,
    last: u64,
    /// The length of the last segment's whole records.
    active_len: u64,
    /// Where the last segment's torn tail starts, if it has one.
    torn_at: Option<u64>,
}

/// Reads the directory without changing it (see [`Store::open`] for the rules).
fn scan<F: Fs>(fs: &F) -> Result<Scan, StoreError> {
    let names = fs.list()?;
    let mut segments = Vec::new();
    let mut has_hard = false;
    let mut has_snapshot = false;
    let mut leftovers = Vec::new();
    for name in names {
        if name == HARD_STATE {
            has_hard = true;
        } else if name == SNAPSHOT {
            has_snapshot = true;
        } else if name == HARD_STATE_TMP {
            leftovers.push(HARD_STATE_TMP);
        } else if name == SNAPSHOT_TMP {
            leftovers.push(SNAPSHOT_TMP);
        } else if name.ends_with(".log") {
            let (seq, first) =
                parse_segment_name(&name).ok_or_else(|| corrupt(&name, 0, "bad segment name"))?;
            if first == 0 {
                return Err(corrupt(&name, 0, "segment does not continue the log"));
            }
            segments.push(Segment { seq, first, name });
        }
    }
    segments.sort_by_key(|s| s.seq);
    // A segment that a later one replaces whole is not part of the log, whatever
    // it holds: the removals after a truncation reach the disk in any order.
    let next_seq = segments.last().map_or(1, |s| s.seq + 1);
    let mut dead = Vec::new();
    let mut live: Vec<Segment> = Vec::new();
    for seg in segments.into_iter().rev() {
        if live.last().is_some_and(|later| later.first <= seg.first) {
            dead.push(seg);
        } else {
            live.push(seg);
        }
    }
    live.reverse();
    let mut segments = live;

    let hard = if has_hard {
        let bytes = fs.read(HARD_STATE)?;
        match get_record(&bytes) {
            Some((payload, used)) if used == bytes.len() => {
                decode_hard(payload).map_err(|r| corrupt(HARD_STATE, 0, r))?
            }
            _ => return Err(corrupt(HARD_STATE, 0, "hard state record damaged")),
        }
    } else {
        HardState::default()
    };
    let snapshot = if has_snapshot {
        let bytes = fs.read(SNAPSHOT)?;
        Some(decode_snapshot(&bytes).map_err(|(at, r)| corrupt(SNAPSHOT, at, r))?)
    } else {
        None
    };
    let base = snapshot.as_ref().map_or(0, |s| s.base.index.0);
    // Firsts rise along the live segments, so the snapshot covers a prefix of them:
    // every one before the last that starts at or below the entry after the base.
    let covered = segments
        .iter()
        .rposition(|s| s.first <= base + 1)
        .unwrap_or(0);
    dead.extend(segments.drain(..covered));

    let mut entries: Vec<Entry> = Vec::new();
    // The index of the last entry read, those at or below the base included.
    let mut end = 0;
    let mut active_len = 0;
    let mut torn_at = None;
    for (k, seg) in segments.iter().enumerate() {
        // The first segment meets an empty log, so this also requires it to start
        // at index 1, or at or below the entry after the base.
        if seg.first > end.max(base) + 1 {
            return Err(corrupt(&seg.name, 0, "segment does not continue the log"));
        }
        while entries.last().is_some_and(|e| e.id.index.0 >= seg.first) {
            entries.pop();
        }
        end = seg.first - 1;
        let bytes = fs.read(&seg.name)?;
        let is_last = k + 1 == segments.len();
        let mut off = 0;
        while off < bytes.len() {
            let Some((payload, used)) = get_record(&bytes[off..]) else {
                if is_last && !any_record(&bytes[off + 1..]) {
                    torn_at = Some(off as u64);
                    break;
                }
                return Err(corrupt(&seg.name, off, "damaged record"));
            };
            let entry = decode_entry(payload).map_err(|r| corrupt(&seg.name, off, r))?;
            if entry.id.index.0 != end + 1 {
                return Err(corrupt(&seg.name, off, "entry out of order"));
            }
            end += 1;
            if end > base {
                entries.push(entry);
            }
            off += used;
        }
        active_len = off as u64;
    }
    if base > 0 && end <= base {
        // The log ends at or below the base: its last segment holds nothing live.
        dead.extend(segments.pop());
        torn_at = None;
    }
    Ok(Scan {
        stored: Stored {
            hard,
            snapshot,
            entries,
        },
        segments,
        dead,
        leftovers,
        next_seq,
        last: end.max(base),
        active_len,
        torn_at,
    })
}
