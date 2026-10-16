//! What the `Segments` service asks of the cache (`docs/design/write-path.md`, section
//! 4): storing segments the server built from a daemon's checked records under one
//! commit, locating blobs, and reading ranges of stored objects.

use bytes::Bytes;
use futures::future::try_join_all;
use kbf_meta::{BlobAnswer, Location, ObjectId, StoreId};
use kbf_objstore::{ByteRange, ObjectStore, ObjectStoreError};
use kbf_segments::layout::encode_footer;
use kbf_segments::{Footer, IndexEntry};
use kbf_types::Digest;

use super::verify::verify_records;
use super::{Cache, CacheError};
use crate::meta_log::MetaLog;

/// One segment ready to store: its whole bytes (records, then a footer the server
/// built from `entries`) and the entries that footer indexes.
#[derive(Debug)]
pub(crate) struct BuiltSegment {
    pub body: Bytes,
    pub entries: Vec<IndexEntry>,
    /// Whether every record was hashed to its digest, and `entries` carry the CRC-32C
    /// of that pass. An unverified segment (an upload on a write tier, which trusts the
    /// daemon's digests) is staged as it is, and the flusher hashes it before the PUT;
    /// one the tier does not take is hashed here before it is PUT.
    pub verified: bool,
    /// The records' range on the write tier when they were written there as they
    /// arrived (`body` then holds nothing): staged with the declared entries, never PUT
    /// from here.
    pub staged: Option<super::stream::TierRange>,
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// Stores every segment of an upload and commits it. Returns the digests
    /// committed, in segment then index order.
    ///
    /// With a write tier attached, each segment is first offered to `Writer::stage`
    /// as a block of its own: records written and `fdatasync`ed on the tier, then one
    /// `PutStaged` for its blobs. That is the whole of its store: the commit is
    /// answered without an object-store PUT, the blobs are read from the tier, and
    /// the flusher PUTs the object later. A segment the tier does not take (no tier,
    /// at its budget, a disk error before the commit) is stored as a grouped segment
    /// is (`Writer::store_segment`): under a `--write-puts-in-flight` slot, its PUT
    /// retried under a new key, its footer read back, after its records were hashed if
    /// they were not (a segment is never PUT unverified). The PUT segments of the upload
    /// are then indexed by **one** [`PutBlobs`](kbf_meta::Command::PutBlobs).
    ///
    /// Nothing PUT is indexed unless every PUT and read-back succeeded: a failure
    /// leaves the objects written so far as orphans, unreferenced. The segments
    /// already staged stay staged and indexed (a retry of the upload finds them
    /// present).
    ///
    /// # Errors
    /// The store or the metadata log failed; a footer read back without a record the
    /// server put in it ([`CacheError::Internal`]); a record of a segment the tier did
    /// not take that does not hash to its digest ([`CacheError::DigestMismatch`]).
    pub(crate) async fn store_upload(
        &self,
        segments: Vec<BuiltSegment>,
    ) -> Result<Vec<Digest>, CacheError> {
        let digests = segments
            .iter()
            .flat_map(|s| s.entries.iter().map(|e| e.digest))
            .collect();
        let placed = try_join_all(segments.into_iter().map(|s| self.put_built(s))).await?;
        let placed: Vec<(Digest, Location)> = placed.into_iter().flatten().collect();
        if !placed.is_empty() {
            self.writer.commit_segment(placed).await?;
        }
        Ok(digests)
    }

    /// Stages one built segment on the write tier if one takes it (nothing is left to
    /// index: `PutStaged` did), else writes it as a new object and returns where each
    /// of its records is, as the footer read back from the store says.
    async fn put_built(
        &self,
        segment: BuiltSegment,
    ) -> Result<Vec<(Digest, Location)>, CacheError> {
        if let Some(range) = segment.staged {
            let blobs = segment
                .entries
                .iter()
                .map(|e| (e.digest, e.offset))
                .collect();
            self.stage_range(range, blobs).await?;
            return Ok(Vec::new());
        }
        if self.writer.stage(segment.body.clone(), true).await? {
            return Ok(Vec::new());
        }
        let segment = if segment.verified {
            segment
        } else {
            self.verify_built(segment).await?
        };
        let (object, key, footer) = self
            .writer
            .store_segment(&self.group, segment.body, segment.entries.len())
            .await?;
        segment
            .entries
            .iter()
            .map(|entry| {
                let found = footer
                    .find(&entry.digest)
                    .filter(|f| f.offset == entry.offset);
                let found = found.ok_or_else(|| {
                    CacheError::Internal(format!(
                        "segment {key} came back without blob {} at {}",
                        entry.digest, entry.offset
                    ))
                })?;
                Ok((
                    entry.digest,
                    Location {
                        store: StoreId::CONFIGURED,
                        object,
                        offset: found.offset,
                    },
                ))
            })
            .collect()
    }

    /// Hashes every record of an unverified `segment` against its entry's digest, on the
    /// hashing pool, and rebuilds the footer from the CRC-32C of that pass (the
    /// daemon's never reaches the store).
    ///
    /// # Errors
    /// [`CacheError::DigestMismatch`] for the first record that does not hash to its
    /// digest; [`CacheError::Internal`] if the hashing job failed.
    async fn verify_built(&self, segment: BuiltSegment) -> Result<BuiltSegment, CacheError> {
        let BuiltSegment { body, entries, .. } = segment;
        let records_len = Footer::parse(&body, body.len() as u64)?.records_len();
        let work = body.clone();
        let blobs: Vec<(Digest, u64)> = entries.iter().map(|e| (e.digest, e.offset)).collect();
        let found = self
            .hashing()
            .compute(move || {
                let end = usize::try_from(records_len).unwrap_or(usize::MAX);
                verify_records(&work[..end.min(work.len())], &blobs)
            })
            .await?;
        if let Some(bad) = found.bad.first() {
            return Err(CacheError::DigestMismatch {
                claimed: bad.claimed,
                actual: bad.actual,
            });
        }
        // The same entry count as the declared footer, so the same length.
        let footer = encode_footer(records_len, &found.good);
        let records = &body[..usize::try_from(records_len).unwrap_or(body.len())];
        Ok(BuiltSegment {
            body: Bytes::from([records, footer.as_slice()].concat()),
            entries: found.good,
            verified: true,
            staged: None,
        })
    }

    /// Where each of `digests` is, in order: the object and offset of a blob present
    /// and reachable, `None` for any other (the empty blob included: it is in no
    /// object). The present ones are touched before the answer, as a read is, so a
    /// fill that reads them next finds them.
    ///
    /// # Errors
    /// The metadata log is unavailable or kept changing.
    pub(crate) async fn locate(
        &self,
        digests: &[Digest],
    ) -> Result<Vec<Option<Location>>, CacheError> {
        let found = self
            .touched(|s| {
                let found: Vec<Option<Location>> = digests
                    .iter()
                    .map(|d| match s.blob(d) {
                        BlobAnswer::Present { location, .. } => Some(location),
                        BlobAnswer::Unavailable | BlobAnswer::Absent => None,
                    })
                    .collect();
                let present = digests.iter().zip(&found).filter_map(|(d, l)| l.map(|_| d));
                let touch = s.touch_for(present);
                (found, touch)
            })
            .await?;
        for location in found.iter().flatten() {
            self.used([location.object]);
        }
        Ok(found)
    }

    /// `len` bytes of object `object` from `offset`, unchecked: the caller checks each
    /// blob it cuts from them. Read from the write tier when it holds the object (a
    /// staged block, or its flushed copy), else from the store. `None` when the store does not hold the whole range (no
    /// such object, or one too short), which the service answers NOT_FOUND.
    ///
    /// # Errors
    /// The store failed in another way, or the key could not be built.
    pub(crate) async fn read_object_range(
        &self,
        object: ObjectId,
        offset: u64,
        len: u64,
    ) -> Result<Option<Bytes>, CacheError> {
        self.used([object]);
        if let Some(bytes) = self.read_staged_range(object, offset, len).await {
            return Ok(Some(bytes));
        }
        let key = self.object_key(object)?;
        let Some(range) = ByteRange::new(offset, len) else {
            return Ok(None);
        };
        match self.writer.get_range(&key, range).await {
            Ok(bytes) if bytes.len() as u64 == len => Ok(Some(bytes)),
            Ok(_) | Err(ObjectStoreError::NotFound(_) | ObjectStoreError::InvalidRange { .. }) => {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}
