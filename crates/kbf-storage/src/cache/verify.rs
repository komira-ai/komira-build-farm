//! Hashing the records of a block or an unverified segment against the digests the
//! index lists them under (`docs/design/write-path.md`, section 6.7).

use kbf_segments::{IndexEntry, RecordHasher};
use kbf_types::Digest;

/// A record that does not hash to the digest it is listed under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Mismatch {
    /// The digest the record was listed under.
    pub claimed: Digest,
    /// The digest of the bytes found (of no bytes, if the record lies past `records`).
    pub actual: Digest,
}

/// What hashing a block's records found.
#[derive(Debug, Default)]
pub(super) struct Verified {
    /// Every record that hashed to its digest, with its CRC-32C and offset, in the
    /// order of the `blobs` hashed.
    pub good: Vec<IndexEntry>,
    /// Every record that did not.
    pub bad: Vec<Mismatch>,
}

/// Hashes (SHA-256 and CRC-32C) each of `blobs`, digests with their offsets in
/// `records`, and sorts them into [`Verified::good`] and [`Verified::bad`]. A record
/// not wholly inside `records` is bad. CPU-bound: run it on the hashing pool.
pub(super) fn verify_records(records: &[u8], blobs: &[(Digest, u64)]) -> Verified {
    let mut check = RecordCheck::new(blobs.to_vec());
    check.update(0, records);
    check.finish()
}

/// [`verify_records`] of records whose bytes arrive in pieces, in order: each piece is
/// hashed into the records it covers as it comes, and nothing is kept but one hasher.
/// The pieces need not line up with the records, and bytes no record lists (between
/// them) are passed over.
#[derive(Debug, Default)]
pub(super) struct RecordCheck {
    blobs: Vec<(Digest, u64)>,
    /// Indices into `blobs`, by offset.
    order: Vec<usize>,
    /// Where in `order` the record being hashed is.
    pos: usize,
    hasher: RecordHasher,
    found: Vec<Option<Result<IndexEntry, Mismatch>>>,
}

impl RecordCheck {
    /// A check of `blobs`, digests with their offsets. Records must not overlap.
    pub(super) fn new(blobs: Vec<(Digest, u64)>) -> Self {
        let mut order: Vec<usize> = (0..blobs.len()).collect();
        order.sort_by_key(|&i| blobs[i].1);
        let found = vec![None; blobs.len()];
        Self {
            blobs,
            order,
            pos: 0,
            hasher: RecordHasher::new(),
            found,
        }
    }

    /// Hashes `piece`, the bytes from offset `at`, into the records it covers. Pieces
    /// come in order, each starting where the one before ended.
    pub(super) fn update(&mut self, at: u64, piece: &[u8]) {
        let end = at + piece.len() as u64;
        while let Some(&i) = self.order.get(self.pos) {
            let (digest, offset) = self.blobs[i];
            if offset > end || (offset == end && digest.size_bytes > 0) {
                break;
            }
            let record_end = offset.saturating_add(digest.size_bytes);
            let (from, to) = (offset.max(at), record_end.min(end));
            if from < to {
                // Both within `piece`, which is in memory.
                self.hasher
                    .update(&piece[(from - at) as usize..(to - at) as usize]);
            }
            if record_end > end {
                break;
            }
            self.close(i);
        }
    }

    /// Ends the record being hashed: good if it hashed to its digest.
    fn close(&mut self, i: usize) {
        let (claimed, offset) = self.blobs[i];
        let (actual, crc32c) = std::mem::take(&mut self.hasher).finish();
        self.found[i] = Some(if actual == claimed {
            Ok(IndexEntry {
                digest: claimed,
                offset,
                crc32c,
            })
        } else {
            Err(Mismatch { claimed, actual })
        });
        self.pos += 1;
    }

    /// What the pieces showed: a record the pieces did not wholly cover is bad, with
    /// the digest of what of it came (none, for one past the end).
    pub(super) fn finish(mut self) -> Verified {
        while let Some(&i) = self.order.get(self.pos) {
            self.close(i);
        }
        let mut out = Verified::default();
        for found in self.found.into_iter().flatten() {
            match found {
                Ok(entry) => out.good.push(entry),
                Err(bad) => out.bad.push(bad),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(bytes: &[u8], at: u64) -> (Digest, u64) {
        (kbf_segments::sha256(bytes), at)
    }

    /// Catches a check that stops at the first bad record (the good ones after it
    /// lost), one that passes a record whose bytes changed, one that panics or passes
    /// a record running past the block, and a CRC taken of other bytes than the
    /// record's.
    #[test]
    fn records_are_sorted_into_good_and_bad_by_their_own_bytes() {
        let records = b"firstSECONDthird".to_vec();
        let list = [blob(b"first", 0), blob(b"seconX", 5), blob(b"third", 11)];
        let found = verify_records(&records, &list);
        let good: Vec<_> = found.good.iter().map(|e| (e.digest, e.offset)).collect();
        assert_eq!(good, [list[0], list[2]]);
        assert_eq!(found.good[1].crc32c, kbf_segments::crc32c(b"third"));
        assert_eq!(found.bad.len(), 1);
        assert_eq!(found.bad[0].claimed, list[1].0);
        assert_eq!(found.bad[0].actual, kbf_segments::sha256(b"SECOND"));
        let past = verify_records(&records, &[blob(b"third!", 11), blob(b"x", u64::MAX)]);
        assert!(past.good.is_empty());
        assert_eq!(past.bad.len(), 2);
        assert_eq!(past.bad[1].actual, kbf_segments::sha256(b""));
    }

    /// Catches a piecewise check that differs from the whole one: a record split across
    /// pieces hashed per piece, a piece covering two records hashed into one, bytes
    /// between records hashed into the next, or the order of the answers changed (the
    /// footer is built from `good` as listed).
    #[test]
    fn pieces_check_as_the_whole_does() {
        let records = b"first..SECONDthird".to_vec();
        let list = [blob(b"third", 13), blob(b"first", 0), blob(b"SECOND", 7)];
        let whole = verify_records(&records, &list);
        for cut in [1, 3, 5, 7, 8, 12, 13, 17] {
            let mut check = RecordCheck::new(list.to_vec());
            check.update(0, &records[..cut]);
            check.update(cut as u64, &records[cut..]);
            let pieces = check.finish();
            assert_eq!(pieces.good, whole.good, "cut at {cut}");
            assert!(pieces.bad.is_empty(), "cut at {cut}: {:?}", pieces.bad);
        }
        let mut bytewise = RecordCheck::new(list.to_vec());
        for (at, byte) in records.iter().enumerate() {
            bytewise.update(at as u64, std::slice::from_ref(byte));
        }
        assert_eq!(bytewise.finish().good, whole.good);
        let order: Vec<_> = whole.good.iter().map(|e| (e.digest, e.offset)).collect();
        assert_eq!(order, list, "in the order listed");
        let mut cut_short = RecordCheck::new(list.to_vec());
        cut_short.update(0, &records[..15]);
        let short = cut_short.finish();
        assert_eq!(short.good.len(), 2);
        assert_eq!(short.bad[0].actual, kbf_segments::sha256(b"th"));
    }

    /// Catches an empty record (no bytes, at a piece's end or past the last) left
    /// unchecked or taken for bad: each hashes to the empty digest, wherever it lies.
    #[test]
    fn empty_records_check_wherever_they_lie() {
        let list = [blob(b"ab", 0), blob(b"", 2), blob(b"cd", 2), blob(b"", 4)];
        let mut check = RecordCheck::new(list.to_vec());
        check.update(0, b"ab");
        check.update(2, b"cd");
        let found = check.finish();
        assert!(found.bad.is_empty(), "{:?}", found.bad);
        assert_eq!(found.good.len(), 4);
    }
}
