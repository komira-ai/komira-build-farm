//! Failure-log copies (ui-data.md 5.3 and Q21): a failed run's stdout, stderr and
//! ActionResult kept in one `r30/flog/<day>/<run id>` object, so they outlive the CAS
//! blobs they were copied from.
//!
//! - **Object first, row second.** [`FlogStore::put`] writes the object and returns its
//!   [`FlogRef`]; only then is the ref offered to the writer as [`Event::Flog`], which
//!   queues the `ui/obj/flog/<day>/<run id>` row through the same pending path as a
//!   chunk's rows (a dropped row is restored from the object's footer, with no LIST). A
//!   crash between the two leaves an orphan object, never a row naming an object that was
//!   not written.
//! - **Pieces.** Each stream is stored as pieces of [`PIECE_BYTES`], each its own block,
//!   so a range of a 16 MiB log is read from the blocks that hold it ([`Reader`]
//!   `read_log`), not the object.
//! - **Instance in the summary.** The footer's [`FlogSummary`] names the run's instance, so
//!   the reader refuses a caller `allow` does not accept before reading a byte of a log.
//!
//! [`Event::Flog`]: crate::Event::Flog
//! [`Reader`]: crate::Reader

use kbf_objstore::{ObjectKey, ObjectStore, ObjectStoreError};
use prost::Message;

use crate::container::{self, Encoded, Header};
use crate::keys::{Family, HistoryRoot, WriterId};
use crate::run_id::RunId;
use crate::scoped::Scoped;

/// Why a failed run's failure-log object lacks a log it should hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum FlogOmitted {
    /// Every log the run has was copied (or none was due).
    No = 0,
    /// A log was over [`FLOG_STREAM_BYTES`] (or too large to read at all), so it was
    /// not copied; its tail is still kept.
    TooLarge = 1,
}

/// The most bytes of one stream (stdout or stderr) a failure-log object holds: a larger
/// stream is not copied, and the run says `flog_omitted: "too_large"`.
pub const FLOG_STREAM_BYTES: u64 = 16 * 1024 * 1024;

/// Which log of a run a failure-log object record holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum FlogStream {
    /// The command's standard output.
    Stdout = 0,
    /// The command's standard error.
    Stderr = 1,
    /// The ActionResult, as the blob it was stored in.
    Result = 2,
}

impl FlogStream {
    /// The name the API gives the stream.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            FlogStream::Stdout => "stdout",
            FlogStream::Stderr => "stderr",
            FlogStream::Result => "result",
        }
    }

    /// The stream the API calls `name`.
    #[must_use]
    pub fn named(name: &str) -> Option<FlogStream> {
        [FlogStream::Stdout, FlogStream::Stderr, FlogStream::Result]
            .into_iter()
            .find(|s| s.name() == name)
    }
}

/// One piece of a stream in a failure-log object: at most `flog::PIECE_BYTES` bytes at
/// `offset`, so a range of a log is read from the blocks that hold it.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct FlogRecord {
    /// Which stream.
    #[prost(enumeration = "FlogStream", tag = "1")]
    pub stream: i32,
    /// Where in the stream the piece starts.
    #[prost(uint64, tag = "2")]
    pub offset: u64,
    /// The piece.
    #[prost(bytes = "vec", tag = "3")]
    pub bytes: Vec<u8>,
}

/// The sort key of a [`FlogRecord`] in a failure-log object: its stream byte, then its
/// offset, big-endian.
#[must_use]
pub fn flog_key(stream: FlogStream, offset: u64) -> [u8; 9] {
    let mut k = [stream as u8; 9];
    k[1..].copy_from_slice(&offset.to_be_bytes());
    k
}

/// The family summary of a failure-log object's footer: whose it is and how long each
/// log it holds is, so a row lost after the PUT is restored from the object with no
/// LIST, and a read is refused for an instance the caller may not read before any byte
/// of a log is.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct FlogSummary {
    /// The run's id, 12 bytes.
    #[prost(bytes = "vec", tag = "1")]
    pub run_id: Vec<u8>,
    /// The `yyyymmdd` day in the object's key.
    #[prost(uint32, tag = "2")]
    pub day: u32,
    /// The instance the run was asked under.
    #[prost(string, tag = "3")]
    pub instance: String,
    /// The length of stdout, if the object holds it.
    #[prost(uint64, optional, tag = "4")]
    pub stdout_len: Option<u64>,
    /// The length of stderr, if the object holds it.
    #[prost(uint64, optional, tag = "5")]
    pub stderr_len: Option<u64>,
    /// The length of the ActionResult blob, if the object holds it.
    #[prost(uint64, optional, tag = "6")]
    pub result_len: Option<u64>,
}

impl FlogSummary {
    /// The length of `stream`'s log if the object holds it.
    #[must_use]
    pub fn len_of(&self, stream: FlogStream) -> Option<u64> {
        match stream {
            FlogStream::Stdout => self.stdout_len,
            FlogStream::Stderr => self.stderr_len,
            FlogStream::Result => self.result_len,
        }
    }
}

/// The `ui/obj/<family>/<day>/<name>` row: where an object's footer is, so a read takes
/// the footer with one exact range read, as an `ichunk` row's does.
#[derive(Clone, PartialEq, Eq, prost::Message)]
pub struct ObjRow {
    /// Where the footer starts.
    #[prost(uint64, tag = "1")]
    pub footer_offset: u64,
    /// The footer's length with the trailer.
    #[prost(uint32, tag = "2")]
    pub footer_len: u32,
    /// The whole object's length.
    #[prost(uint64, tag = "3")]
    pub bytes: u64,
}

/// The most bytes of a stream one record (and one block) holds.
pub const PIECE_BYTES: usize = 64 * 1024;

/// The logs of a run to copy: each is `None` when the run has no such log, or it is not
/// to be copied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlogParts<'a> {
    /// The instance the run was asked under: who may read the copy.
    pub instance: &'a str,
    /// The stdout bytes.
    pub stdout: Option<&'a [u8]>,
    /// The stderr bytes.
    pub stderr: Option<&'a [u8]>,
    /// The ActionResult blob.
    pub result: Option<&'a [u8]>,
}

impl FlogParts<'_> {
    /// Whether there is nothing to copy: no object is written for such a run.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stdout.is_none() && self.stderr.is_none() && self.result.is_none()
    }
}

/// An encoded failure-log object and its key, ready to be put.
#[derive(Debug)]
pub struct FlogObject {
    run: RunId,
    key: ObjectKey,
    encoded: Encoded,
}

impl FlogObject {
    /// The object's length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.encoded.bytes.len()
    }

    /// Never empty: an object has at least a header and a trailer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.encoded.bytes.is_empty()
    }

    /// The object's key.
    #[must_use]
    pub fn key(&self) -> &ObjectKey {
        &self.key
    }

    /// What the writer needs to put the object's index row.
    fn reference(&self) -> FlogRef {
        FlogRef {
            run: self.run,
            key: self.key.clone(),
            footer_offset: self.encoded.footer_offset,
            footer_len: self.encoded.footer_len,
            bytes: self.encoded.bytes.len() as u64,
        }
    }
}

/// A failure-log object that was written: what its index row holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlogRef {
    /// The run the copy is of.
    pub run: RunId,
    /// The object's key.
    pub key: ObjectKey,
    /// Where its footer starts.
    pub footer_offset: u64,
    /// The footer's length with the trailer.
    pub footer_len: u32,
    /// The object's length.
    pub bytes: u64,
}

/// Writes failure-log objects under `<root>history/`, through a [`Scoped`] store, so a
/// key outside it is refused.
pub struct FlogStore<O> {
    objects: Scoped<O>,
    root: HistoryRoot,
    writer: WriterId,
}

impl<O: ObjectStore> FlogStore<O> {
    /// A store of copies written by `writer` under `root` in `objects`.
    #[must_use]
    pub fn new(objects: O, root: HistoryRoot, writer: WriterId) -> FlogStore<O> {
        FlogStore {
            objects: Scoped::history(objects, &root),
            root,
            writer,
        }
    }

    /// The object holding `parts` for run `run`. CPU work, proportional to the bytes: a
    /// caller on an async runtime runs it where blocking is allowed.
    #[must_use]
    pub fn encode(&self, run: RunId, parts: &FlogParts<'_>) -> FlogObject {
        let mut records = Vec::new();
        let mut lens = [None; 3];
        let streams = [
            (FlogStream::Stdout, parts.stdout),
            (FlogStream::Stderr, parts.stderr),
            (FlogStream::Result, parts.result),
        ];
        for (at, (stream, bytes)) in streams.into_iter().enumerate() {
            let Some(bytes) = bytes else {
                continue;
            };
            lens[at] = Some(bytes.len() as u64);
            for (n, piece) in bytes.chunks(PIECE_BYTES).enumerate() {
                let offset = (n * PIECE_BYTES) as u64;
                let record = FlogRecord {
                    stream: stream.into(),
                    offset,
                    bytes: piece.to_vec(),
                };
                records.push((flog_key(stream, offset).to_vec(), record.encode_to_vec()));
            }
        }
        let summary = FlogSummary {
            run_id: run.to_bytes().to_vec(),
            day: run.day().number(),
            instance: parts.instance.to_owned(),
            stdout_len: lens[0],
            stderr_len: lens[1],
            result_len: lens[2],
        };
        let header = Header {
            family: Family::FailureLog,
            writer: self.writer,
            day: run.day(),
            seq: 0,
            base_ms: run.time_ms(),
        };
        FlogObject {
            run,
            key: self.root.flog(run),
            encoded: container::encode(&header, records, summary.encode_to_vec()),
        }
    }

    /// Writes `object` (a key is never reused, so `AlreadyExists` on a retry is success)
    /// and returns what its index row holds. Nothing is queued for the index: the caller
    /// offers the returned ref to the writer once this has returned.
    ///
    /// # Errors
    /// [`ObjectStoreError`] when the store refuses or fails the write.
    pub async fn put(&self, object: FlogObject) -> Result<FlogRef, ObjectStoreError> {
        let reference = object.reference();
        let body = bytes::Bytes::from(object.encoded.bytes);
        match self.objects.put_new(&object.key, body, None).await {
            Ok(()) | Err(ObjectStoreError::AlreadyExists(_)) => Ok(reference),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use prost::Message as _;

    use super::*;
    use crate::records::{FailedRun, TailOmitted};

    #[test]
    fn flog_streams_have_names_keys_and_lengths_that_cannot_cross() {
        // Catches: a stream name the API and the object disagree on, keys that sort a
        // later piece before an earlier one or one stream's pieces among another's (a
        // range read would assemble the wrong bytes), a length read from another stream's
        // field, and a failed run that reads back as one with logs omitted.
        for s in [FlogStream::Stdout, FlogStream::Stderr, FlogStream::Result] {
            assert_eq!(FlogStream::named(s.name()), Some(s));
        }
        assert_eq!(FlogStream::named("stdin"), None);
        let at = |s, o| flog_key(s, o);
        assert!(at(FlogStream::Stdout, 9) < at(FlogStream::Stdout, 1 << 16));
        assert!(at(FlogStream::Stdout, u64::MAX) < at(FlogStream::Stderr, 0));
        assert!(at(FlogStream::Stderr, u64::MAX) < at(FlogStream::Result, 0));
        let summary = FlogSummary {
            run_id: Vec::new(),
            day: 0,
            instance: String::new(),
            stdout_len: Some(1),
            stderr_len: Some(2),
            result_len: None,
        };
        assert_eq!(summary.len_of(FlogStream::Stdout), Some(1));
        assert_eq!(summary.len_of(FlogStream::Stderr), Some(2));
        assert_eq!(summary.len_of(FlogStream::Result), None);
        assert_eq!(FailedRun::default().flog_omitted(), FlogOmitted::No);
        assert_eq!(TailOmitted::Busy as i32, 3);
        // A record written before the field existed reads with none omitted.
        let old = FailedRun {
            seq: 4,
            ..FailedRun::default()
        };
        assert_eq!(
            FailedRun::decode(old.encode_to_vec().as_slice()).unwrap(),
            old
        );
    }
}
