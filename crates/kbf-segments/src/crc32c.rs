//! CRC-32C (Castagnoli), the per-record and footer checksum of a segment.
//!
//! Computed by the `crc32c` crate: the CPU's CRC32C instructions (SSE4.2 on x86-64, the
//! CRC extension on aarch64) where the CPU has them, a portable table otherwise. The
//! value is the same either way, and the same as the byte-at-a-time table this module
//! had before (`tests::agrees_with_the_bitwise_definition` pins it), so segments
//! written before and after read alike.

/// The CRC-32C of `bytes`.
#[must_use]
pub fn crc32c(bytes: &[u8]) -> u32 {
    ::crc32c::crc32c(bytes)
}

/// A CRC-32C over bytes that arrive in pieces: equal to [`crc32c`] of the pieces
/// joined.
#[derive(Clone, Copy, Debug)]
pub struct Crc32c(u32);

impl Crc32c {
    /// The CRC of no bytes yet.
    #[must_use]
    pub const fn new() -> Self {
        Self(0)
    }

    /// Adds `bytes` after those added before.
    pub fn update(&mut self, bytes: &[u8]) {
        // The crate's running value is the finished CRC of the bytes so far.
        self.0 = ::crc32c::crc32c_append(self.0, bytes);
    }

    /// The CRC-32C of every byte added.
    #[must_use]
    pub const fn finish(self) -> u32 {
        self.0
    }
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::crc32c;

    /// Catches: a wrong polynomial (plain CRC-32 gives 0xcbf43926 for "123456789"), a
    /// missing initial or final inversion, or a non-reflected table. The vectors are the
    /// standard check value and the RFC 3720 (iSCSI) test patterns.
    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8a91_36aa);
        assert_eq!(crc32c(&[0xffu8; 32]), 0x62a8_ab43);
        let ascending: Vec<u8> = (0u8..32).collect();
        assert_eq!(crc32c(&ascending), 0x46dd_794e);
    }

    /// The definition, one bit at a time: the oracle for the fast path.
    fn bitwise(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0x82f6_3b78
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// Catches a fast path that differs from the definition at any length (the
    /// hardware loop takes 8 bytes at a time, with a head and a tail), at any
    /// alignment, or when fed in pieces: every length to 300, from five starting
    /// offsets, one-shot and split in the middle.
    #[test]
    fn agrees_with_the_bitwise_definition() {
        let data: Vec<u8> = (0..400u32).map(|i| (i * 7 + i / 3) as u8).collect();
        for start in 0..5 {
            for len in 0..300 {
                let bytes = &data[start..start + len];
                assert_eq!(crc32c(bytes), bitwise(bytes), "start {start} len {len}");
                let mut crc = super::Crc32c::new();
                crc.update(&bytes[..len / 2]);
                crc.update(&bytes[len / 2..]);
                assert_eq!(
                    crc.finish(),
                    bitwise(bytes),
                    "split start {start} len {len}"
                );
            }
        }
    }

    /// Catches an incremental CRC that restarts, or inverts, at each piece: any split of
    /// the bytes gives the one-shot value.
    #[test]
    fn pieces_give_the_whole_crc() {
        let bytes: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        for split in [0, 1, 7, 500, 999, 1000] {
            let mut crc = super::Crc32c::default();
            crc.update(&bytes[..split]);
            crc.update(&bytes[split..]);
            assert_eq!(crc.finish(), crc32c(&bytes), "split at {split}");
        }
    }
}
