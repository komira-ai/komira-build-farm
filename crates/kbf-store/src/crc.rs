//! CRC-32C (Castagnoli, reflected polynomial 0x82F63B78), the checksum of every record.
//! A byte-at-a-time table: the log is fsync-bound, not checksum-bound.

const POLY: u32 = 0x82F6_3B78;

const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        #[allow(clippy::cast_possible_truncation)]
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 { (c >> 1) ^ POLY } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// The CRC-32C of `parts`, concatenated.
pub fn crc32c(parts: &[&[u8]]) -> u32 {
    let mut c = !0u32;
    for part in parts {
        for &b in *part {
            c = TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
        }
    }
    !c
}

#[cfg(test)]
mod tests {
    use super::crc32c;

    /// Catches: a wrong polynomial or reflection (the check values are RFC 3720's
    /// appendix B.4 and the common "123456789" check), and a split input that does not
    /// checksum as its concatenation.
    #[test]
    fn known_check_values() {
        assert_eq!(crc32c(&[b"123456789"]), 0xE306_9283);
        assert_eq!(crc32c(&[&[0u8; 32]]), 0x8A91_36AA);
        assert_eq!(crc32c(&[&[0xFFu8; 32]]), 0x62A8_AB43);
        assert_eq!(crc32c(&[b"1234", b"56789"]), 0xE306_9283);
        assert_eq!(crc32c(&[]), 0);
    }
}
