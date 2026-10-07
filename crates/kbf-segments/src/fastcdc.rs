//! FastCDC content-defined chunking for large blobs (RFC section 9.4).
//!
//! The algorithm is FastCDC (Xia et al., USENIX ATC 2016): a gear rolling hash, cut
//! points skipped below the minimum size, and normalized chunking at level 1 (a harder
//! mask before the average size, an easier one after). Because a cut depends only on
//! the bytes just before it, an edit moves the boundaries near it and nowhere else, so
//! two versions of a file share every chunk away from the edit.
//!
//! The gear table, the masks and the sizes are part of the stored format: changing any
//! of them moves every boundary and ends sharing with chunks already stored. The golden
//! test below pins them.

use kbf_types::Digest;

/// Blobs of this size or more are chunked.
pub const CHUNKING_THRESHOLD: u64 = 8 << 20;
/// No chunk but a blob's last is shorter than this.
pub const MIN_CHUNK: usize = 128 << 10;
/// The target average chunk size.
pub const AVG_CHUNK: usize = 512 << 10;
/// No chunk is longer than this.
pub const MAX_CHUNK: usize = 2 << 20;

/// log2(`AVG_CHUNK`).
const AVG_BITS: u32 = AVG_CHUNK.trailing_zeros();
/// Used before the average size: one bit more than the average, so cuts are rarer.
const MASK_SMALL: u64 = top_bits(AVG_BITS + 1);
/// Used from the average size on: one bit fewer, so cuts are likelier.
const MASK_LARGE: u64 = top_bits(AVG_BITS - 1);

/// A mask of the `n` most significant bits. The gear hash shifts left, so the top bits
/// depend on the widest window of recent bytes.
const fn top_bits(n: u32) -> u64 {
    !0u64 << (64 - n)
}

/// 256 fixed pseudo-random words, generated at compile time by splitmix64 from seed 0.
/// A constant, not a random source: every build computes the same table.
const GEAR: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut state = 0u64;
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
};

/// The length of the first chunk of `data`.
fn cut(data: &[u8]) -> usize {
    if data.len() <= MIN_CHUNK {
        return data.len();
    }
    let end = data.len().min(MAX_CHUNK);
    let normal = end.min(AVG_CHUNK);
    let mut hash = 0u64;
    let mut i = MIN_CHUNK;
    while i < normal {
        hash = (hash << 1).wrapping_add(GEAR[usize::from(data[i])]);
        if hash & MASK_SMALL == 0 {
            return i + 1;
        }
        i += 1;
    }
    while i < end {
        hash = (hash << 1).wrapping_add(GEAR[usize::from(data[i])]);
        if hash & MASK_LARGE == 0 {
            return i + 1;
        }
        i += 1;
    }
    end
}

/// The chunks of `data`, in order; together they are exactly `data`. Empty input has
/// no chunks.
#[must_use]
pub fn chunks(data: &[u8]) -> Chunks<'_> {
    Chunks { rest: data }
}

/// An iterator over the content-defined chunks of a byte slice. See [`chunks`].
#[derive(Clone, Debug)]
pub struct Chunks<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        let (chunk, rest) = self.rest.split_at(cut(self.rest));
        self.rest = rest;
        Some(chunk)
    }
}

/// A chunked blob: its own digest and its chunks' digests, in order. Each chunk is
/// stored as an ordinary CAS blob; the blob is the concatenation of its chunks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The digest of the whole blob.
    pub blob: Digest,
    /// The digests of its chunks, in order.
    pub chunks: Vec<Digest>,
}

impl Manifest {
    /// Chunks `data` and hashes the blob and every chunk.
    #[must_use]
    pub fn build(data: &[u8]) -> Self {
        Self {
            blob: crate::sha256(data),
            chunks: chunks(data).map(crate::sha256).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Deterministic test bytes (xorshift64*), so every run chunks the same input.
    fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            out.extend_from_slice(&x.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Catches: chunks that drop, repeat or reorder bytes; a chunk under the minimum
    /// before the last; a chunk over the maximum; an empty chunk.
    #[test]
    fn chunks_tile_the_input_within_bounds() {
        for len in [0, 1, MIN_CHUNK, MIN_CHUNK + 1, 9 << 20] {
            let data = pseudo_random(len, 7);
            let parts: Vec<&[u8]> = chunks(&data).collect();
            assert_eq!(parts.concat(), data, "len {len}");
            for (i, part) in parts.iter().enumerate() {
                assert!(!part.is_empty() && part.len() <= MAX_CHUNK, "len {len}");
                if i + 1 < parts.len() {
                    assert!(part.len() >= MIN_CHUNK, "len {len} chunk {i}");
                }
            }
        }
        // A run of one byte never satisfies the mask, so it is cut at the maximum.
        let zeros = vec![0u8; 5 << 20];
        let lens: Vec<usize> = chunks(&zeros).map(<[u8]>::len).collect();
        assert_eq!(lens, [MAX_CHUNK, MAX_CHUNK, 1 << 20]);
    }

    /// Catches: fixed-size chunking, or any cut rule that depends on the position in
    /// the file rather than on nearby content. An insertion shifts everything after it;
    /// content-defined boundaries resynchronize within a chunk or two, so the new
    /// version shares every chunk away from the edit. Fixed-size chunks after the edit
    /// all change.
    #[test]
    fn small_edit_shares_unchanged_chunks() {
        let original = pseudo_random(16 << 20, 1);
        let at = 7 << 20;
        let mut edited = original[..at].to_vec();
        edited.extend_from_slice(b"an inserted line\n");
        edited.extend_from_slice(&original[at..]);

        let before = Manifest::build(&original);
        let after = Manifest::build(&edited);
        assert!(before.chunks.len() >= 16, "{} chunks", before.chunks.len());
        let old: BTreeSet<Digest> = before.chunks.iter().copied().collect();
        let new: BTreeSet<Digest> = after.chunks.iter().copied().collect();
        let added = new.difference(&old).count();
        let lost = old.difference(&new).count();
        assert!(added <= 2, "{added} new chunks of {}", after.chunks.len());
        assert!(lost <= 2, "{lost} lost chunks of {}", before.chunks.len());
    }

    /// Catches: any change to the gear table, the masks, the sizes or the cut loop.
    /// Each moves boundaries and silently ends sharing with chunks already stored, so
    /// such a change must be deliberate and must move this golden in the same commit.
    #[test]
    fn boundaries_are_pinned() {
        let data = pseudo_random(8 << 20, 42);
        let lens: Vec<usize> = chunks(&data).map(<[u8]>::len).collect();
        assert_eq!(lens, GOLDEN_LENS);
    }

    const GOLDEN_LENS: &[usize] = &[
        594_502, 572_370, 185_211, 782_450, 477_148, 699_907, 718_395, 461_052, 609_445, 685_185,
        1_059_285, 211_231, 732_170, 600_257,
    ];

    /// Catches: a manifest whose chunk digests are not those of the chunks, or whose
    /// blob digest is not that of the whole input.
    #[test]
    fn manifest_names_blob_and_chunks() {
        let data = pseudo_random(3 << 20, 3);
        let m = Manifest::build(&data);
        assert_eq!(m.blob, crate::sha256(&data));
        let expected: Vec<Digest> = chunks(&data).map(crate::sha256).collect();
        assert_eq!(m.chunks, expected);
        assert_eq!(
            m.chunks.iter().map(|d| d.size_bytes).sum::<u64>(),
            data.len() as u64
        );
    }
}
