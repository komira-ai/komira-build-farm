//! The log and the hard state in one directory.

use std::io;

use kbf_raft::{Entry, HardState};
use thiserror::Error;

use crate::fs::{Fs, FsFile};
use crate::record::{
    MAX_PAYLOAD, any_record, decode_entry, decode_hard, encode_entry, encode_hard, get_record,
    put_record,
};

/// The file that holds the hard state.
pub const HARD_STATE: &str = "hardstate";

/// The file a new hard state is written to before it is renamed over [`HARD_STATE`].
pub const HARD_STATE_TMP: &str = "hardstate.tmp";

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
    /// every later call returns [`StoreError::Failed`], and the process must exit
    /// and reopen the store, which reads back only what is on the disk.
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

/// What [`Store::open`] read back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    /// The last hard state persisted, or the default if none ever was.
    pub hard: HardState,
    /// The log, from index 1.
    pub entries: Vec<Entry>,
}

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
/// for bytes that were dropped, so the store never acknowledges anything again.
#[derive(Debug)]
pub struct Store<F: Fs> {
    fs: F,
    options: Options,
    /// The segments still holding live entries, in sequence order.
    segments: Vec<Segment>,
    /// The last segment, open for appending, and its length in bytes.
    active: Option<(F::File, u64)>,
    next_seq: u64,
    last: u64,
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
    /// Before returning, the last segment and the directory are synced, so what open
    /// returns is durable even if the process that wrote it died before its own
    /// sync; then the skipped segments are removed and a leftover
    /// [`HARD_STATE_TMP`] is removed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Corrupt`] for damage other than a torn tail (including a log
    /// that does not start at index 1 or has a gap between segments), and
    /// [`StoreError::Io`] if a filesystem call fails.
    pub fn open(fs: F, options: Options) -> Result<(Self, Recovered), StoreError> {
        let names = fs.list()?;
        let mut segments = Vec::new();
        let mut has_hard = false;
        let mut has_tmp = false;
        for name in names {
            if name == HARD_STATE {
                has_hard = true;
            } else if name == HARD_STATE_TMP {
                has_tmp = true;
            } else if name.ends_with(".log") {
                let (seq, first) = parse_segment_name(&name)
                    .ok_or_else(|| corrupt(&name, 0, "bad segment name"))?;
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
        let segments = live;

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

        let mut entries: Vec<Entry> = Vec::new();
        let mut active_len = 0;
        let mut torn_at = None;
        for (k, seg) in segments.iter().enumerate() {
            let last = entries.len() as u64;
            // The first segment meets an empty log, so this also requires it to start
            // at index 1.
            if seg.first == 0 || seg.first > last + 1 {
                return Err(corrupt(&seg.name, 0, "segment does not continue the log"));
            }
            entries.truncate(usize::try_from(seg.first - 1).map_err(io::Error::other)?);
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
                if entry.id.index.0 != entries.len() as u64 + 1 {
                    return Err(corrupt(&seg.name, off, "entry out of order"));
                }
                entries.push(entry);
                off += used;
            }
            active_len = off as u64;
        }

        let mut active = None;
        if let Some(seg) = segments.last() {
            let mut file = fs.open_append(&seg.name)?;
            if let Some(at) = torn_at {
                file.truncate(at)?;
            }
            file.sync()?;
            active = Some((file, active_len));
        }
        fs.sync_dir()?;
        for seg in dead {
            fs.remove(&seg.name)?;
        }
        if has_tmp {
            fs.remove(HARD_STATE_TMP)?;
        }

        let store = Self {
            fs,
            options,
            segments,
            active,
            next_seq,
            last: entries.len() as u64,
            failed: false,
        };
        Ok((store, Recovered { hard, entries }))
    }

    /// The index of the last entry in the log; 0 when it is empty.
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
    /// is empty, not numbered consecutively, starts at 0 or past the index after the
    /// log's last, or holds an entry whose record would exceed 64 MiB;
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
        if first == 0 || first > self.last + 1 {
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
