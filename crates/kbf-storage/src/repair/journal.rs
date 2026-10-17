//! The repair journal: an append-only file of [`RepairRecord`]s owed and settled.
//!
//! ```text
//! file   = magic frame*
//! magic  = "KBFRPJ01"
//! frame  = len:u32le  crc32c(payload):u32le  payload
//! owed   = 0x01 sha256:32  size:u64le  replica:u64le  generation:u64le
//! done   = 0x02 sha256:32  size:u64le
//! ```
//!
//! What a record is and is not:
//! - **Owed is durable before it returns.** [`Journal::owed`] appends the frame and
//!   syncs the file; the write that needed the record is acknowledged only after.
//! - **Done is not synced.** [`Journal::done`] appends a frame and returns. A power cut
//!   can bring the debt back, and whoever repairs it finds the second copy already there
//!   and settles it again. A debt dropped by mistake could not be got back, so the two
//!   mistakes are not symmetric: only the cheap one is allowed.
//! - **A torn tail is cut, damage is not.** At open, a frame that is short or fails its
//!   CRC is a torn tail only if no whole frame follows it anywhere in the file (bytes
//!   past the last sync are the only ones a power cut can tear, and nothing past it was
//!   acknowledged): the file is cut back to the last whole frame. A bad frame with a
//!   whole one after it, a file that is not a journal, or a whole frame that says
//!   something this code does not know, refuses the open ([`JournalError::Corrupt`]):
//!   guessing past it could forget debts.
//! - **An I/O error stops the journal.** It returns the error and then refuses every
//!   call ([`JournalError::Failed`]); a retried sync can report success for bytes the
//!   kernel dropped, so nothing is acknowledged after one. A reopen after the machine
//!   restarts reads what is on the disk.
//! - **Compaction** rewrites the owed records alone into [`JOURNAL_TMP`], syncs it,
//!   renames it over [`JOURNAL`] and syncs the directory, once the file holds at least
//!   [`Options::compact_frames`] frames and more than twice as many as there are
//!   debts. A crash leaves the old file or the new one, never a mixture; a leftover
//!   temporary file is removed at open.
//!
//! All disk access goes through [`kbf_store::Fs`], so a test can fail or crash it at any
//! operation ([`kbf_store::FaultFs`]).

use std::collections::BTreeMap;
use std::io;

use kbf_map::MemberId;
use kbf_store::{Fs, FsFile};
use kbf_types::{Digest, DigestFunction};

use super::RepairRecord;

/// The journal's file in its directory.
pub const JOURNAL: &str = "repair.journal";

/// The file a compaction writes before it renames it over [`JOURNAL`].
pub const JOURNAL_TMP: &str = "repair.journal.tmp";

const MAGIC: &[u8; 8] = b"KBFRPJ01";
const FRAME_HEADER: usize = 8;
const OWED: u8 = 1;
const DONE: u8 = 2;
const OWED_PAYLOAD: usize = 1 + 32 + 8 + 8 + 8;
const DONE_PAYLOAD: usize = 1 + 32 + 8;

/// How the journal is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// The fewest frames the file holds before it is compacted (it is then compacted
    /// when it also holds more than twice as many frames as debts).
    pub compact_frames: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            compact_frames: 4096,
        }
    }
}

/// Why a journal call failed.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// A filesystem call failed; the journal has stopped.
    #[error("repair journal: {0}")]
    Io(#[from] io::Error),
    /// The file is damaged somewhere other than a torn tail.
    #[error("repair journal damaged at byte {at}: {why}")]
    Corrupt {
        /// Where the damage starts.
        at: usize,
        /// What is wrong.
        why: &'static str,
    },
    /// An earlier call failed with an I/O error; nothing is acknowledged any more.
    #[error("repair journal stopped after an earlier I/O error")]
    Failed,
}

/// The journal over one directory. One process owns the directory.
pub struct Journal<F: Fs> {
    fs: F,
    file: F::File,
    options: Options,
    owed: BTreeMap<Digest, RepairRecord>,
    /// Frames in the file now.
    frames: u64,
    failed: bool,
}

enum Frame {
    Owed(RepairRecord),
    Done(Digest),
}

impl<F: Fs> Journal<F> {
    /// Opens the journal in `fs`'s directory (creating it), cutting a torn tail and
    /// removing a leftover [`JOURNAL_TMP`]. What it returns is durable.
    ///
    /// # Errors
    /// [`JournalError::Corrupt`] for damage other than a torn tail;
    /// [`JournalError::Io`] if a filesystem call fails.
    pub fn open(fs: F, options: Options) -> Result<Self, JournalError> {
        let names = fs.list()?;
        let has = |name: &str| names.iter().any(|n| n == name);
        let existing = has(JOURNAL);
        let bytes = if existing {
            fs.read(JOURNAL)?
        } else {
            Vec::new()
        };
        let scan = scan(&bytes)?;
        let mut file = if existing {
            fs.open_append(JOURNAL)?
        } else {
            fs.create(JOURNAL)?
        };
        if scan.keep < bytes.len() {
            file.truncate(scan.keep as u64)?;
        }
        if scan.keep == 0 {
            file.append(MAGIC)?;
        }
        file.sync()?;
        fs.sync_dir()?;
        if has(JOURNAL_TMP) {
            fs.remove(JOURNAL_TMP)?;
        }
        Ok(Self {
            fs,
            file,
            options,
            owed: scan.owed,
            frames: scan.frames,
            failed: false,
        })
    }

    /// Records `record` as owed, durably: it is on the disk when this returns. A blob
    /// already owed keeps the record it has.
    ///
    /// # Errors
    /// [`JournalError::Io`] if the append or sync fails (the journal then stops);
    /// [`JournalError::Failed`] if it had stopped before.
    pub fn owed(&mut self, record: RepairRecord) -> Result<(), JournalError> {
        if self.failed {
            return Err(JournalError::Failed);
        }
        if self.owed.contains_key(&record.digest) {
            return Ok(());
        }
        let frame = frame_bytes(&Frame::Owed(record));
        self.stop_on_error(|s| {
            s.file.append(&frame)?;
            s.file.sync()
        })?;
        self.owed.insert(record.digest, record);
        self.frames += 1;
        Ok(())
    }

    /// Records that `digest` is no longer owed. Not synced (see the module docs). A
    /// digest not owed is nothing to do.
    ///
    /// # Errors
    /// [`JournalError::Io`] if the append, or a compaction it triggers, fails (the
    /// journal then stops); [`JournalError::Failed`] if it had stopped before.
    pub fn done(&mut self, digest: &Digest) -> Result<(), JournalError> {
        if self.failed {
            return Err(JournalError::Failed);
        }
        if !self.owed.contains_key(digest) {
            return Ok(());
        }
        let frame = frame_bytes(&Frame::Done(*digest));
        self.stop_on_error(|s| s.file.append(&frame))?;
        self.owed.remove(digest);
        self.frames += 1;
        if self.frames >= self.options.compact_frames && self.frames > 2 * self.owed.len() as u64 {
            self.stop_on_error(Self::compact)?;
            self.frames = self.owed.len() as u64;
        }
        Ok(())
    }

    /// Whether `digest` is owed.
    #[must_use]
    pub fn is_owed(&self, digest: &Digest) -> bool {
        self.owed.contains_key(digest)
    }

    /// Every debt, in digest order.
    pub fn records(&self) -> impl Iterator<Item = &RepairRecord> {
        self.owed.values()
    }

    /// How many debts there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.owed.len()
    }

    /// Whether there are no debts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.owed.is_empty()
    }

    /// Runs `op`; an error from it stops the journal.
    fn stop_on_error<T>(&mut self, op: impl FnOnce(&mut Self) -> io::Result<T>) -> io::Result<T> {
        let result = op(self);
        self.failed |= result.is_err();
        result
    }

    /// Rewrites the file as the debts alone.
    fn compact(&mut self) -> io::Result<()> {
        let mut bytes = MAGIC.to_vec();
        for record in self.owed.values() {
            bytes.extend(frame_bytes(&Frame::Owed(*record)));
        }
        let mut tmp = self.fs.create(JOURNAL_TMP)?;
        tmp.append(&bytes)?;
        tmp.sync()?;
        self.fs.rename(JOURNAL_TMP, JOURNAL)?;
        self.fs.sync_dir()?;
        self.file = self.fs.open_append(JOURNAL)?;
        Ok(())
    }
}

/// What the file says, and how much of it is whole.
struct Scan {
    owed: BTreeMap<Digest, RepairRecord>,
    frames: u64,
    /// Bytes to keep: the magic and the whole frames. 0 for a file the creation of
    /// which a crash cut short (its magic is not all there), which is rewritten.
    keep: usize,
}

fn corrupt(at: usize, why: &'static str) -> JournalError {
    JournalError::Corrupt { at, why }
}

fn scan(bytes: &[u8]) -> Result<Scan, JournalError> {
    if !bytes.starts_with(MAGIC) {
        return if MAGIC.starts_with(bytes) {
            Ok(Scan {
                owed: BTreeMap::new(),
                frames: 0,
                keep: 0,
            })
        } else {
            Err(corrupt(0, "not a repair journal"))
        };
    }
    let mut owed = BTreeMap::new();
    let mut frames = 0;
    let mut at = MAGIC.len();
    while at < bytes.len() {
        let Some((payload, used)) = whole(&bytes[at..]) else {
            if (at + 1..bytes.len()).any(|later| whole(&bytes[later..]).is_some()) {
                return Err(corrupt(at, "damaged frame before whole ones"));
            }
            break;
        };
        match decode(payload) {
            Some(Frame::Owed(record)) => {
                owed.insert(record.digest, record);
            }
            Some(Frame::Done(digest)) => {
                owed.remove(&digest);
            }
            None => return Err(corrupt(at, "a record this code does not know")),
        }
        frames += 1;
        at += used;
    }
    Ok(Scan {
        owed,
        frames,
        keep: at,
    })
}

/// The frame at the start of `bytes`, if it is whole: its payload and its length.
fn whole(bytes: &[u8]) -> Option<(&[u8], usize)> {
    let header = bytes.get(..FRAME_HEADER)?;
    let len = usize::try_from(u32::from_le_bytes([
        header[0], header[1], header[2], header[3],
    ]))
    .ok()
    .filter(|len| (1..=OWED_PAYLOAD).contains(len))?;
    let payload = bytes.get(FRAME_HEADER..FRAME_HEADER + len)?;
    let crc = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
    (crc32c::crc32c(payload) == crc).then_some((payload, FRAME_HEADER + len))
}

fn decode(payload: &[u8]) -> Option<Frame> {
    let digest = |p: &[u8]| {
        let mut hash = [0; 32];
        hash.copy_from_slice(&p[1..33]);
        Digest::new(DigestFunction::Sha256, hash, u64_at(p, 33))
    };
    match (payload.first(), payload.len()) {
        (Some(&OWED), OWED_PAYLOAD) => Some(Frame::Owed(RepairRecord {
            digest: digest(payload),
            replica: MemberId(u64_at(payload, 41)),
            generation: u64_at(payload, 49),
        })),
        (Some(&DONE), DONE_PAYLOAD) => Some(Frame::Done(digest(payload))),
        _ => None,
    }
}

/// The little-endian `u64` at `at`, of a payload already checked long enough.
fn u64_at(payload: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&payload[at..at + 8]);
    u64::from_le_bytes(word)
}

fn frame_bytes(frame: &Frame) -> Vec<u8> {
    let mut payload = Vec::with_capacity(OWED_PAYLOAD);
    let digest = match frame {
        Frame::Owed(record) => {
            payload.push(OWED);
            &record.digest
        }
        Frame::Done(digest) => {
            payload.push(DONE);
            digest
        }
    };
    payload.extend_from_slice(&digest.hash);
    payload.extend_from_slice(&digest.size_bytes.to_le_bytes());
    if let Frame::Owed(record) = frame {
        payload.extend_from_slice(&record.replica.0.to_le_bytes());
        payload.extend_from_slice(&record.generation.to_le_bytes());
    }
    let mut out = Vec::with_capacity(FRAME_HEADER + payload.len());
    // A payload is at most OWED_PAYLOAD bytes.
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32c::crc32c(&payload).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}
