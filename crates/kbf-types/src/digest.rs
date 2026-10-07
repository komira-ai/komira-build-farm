//! Content digests: the key of every blob in the content-addressable store.

use std::fmt;

/// The hash function that produced a [`Digest`].
///
/// Only SHA-256 is supported today. The enum is non-exhaustive so that adding a
/// function later is not a breaking change for code that matches on it.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DigestFunction {
    /// SHA-256, the REAPI default; a 32-byte hash.
    Sha256,
}

/// A content digest: the hash function, the hash, and the size of the content in bytes.
///
/// Equality, ordering and hashing use all three fields. Two digests with the same hash
/// but different sizes are different digests: REAPI treats the size as part of the key,
/// and a store that ignored it would serve a blob of the wrong length.
///
/// The text form ([`fmt::Display`] and [`Digest::parse`]) is `<lowercase hex>/<size>`,
/// the form REAPI uses inside resource names. It does not name the hash function; the
/// caller supplies that, as REAPI does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest {
    /// The function that produced `hash`.
    pub function: DigestFunction,
    /// The raw hash bytes.
    pub hash: [u8; 32],
    /// The size of the content in bytes.
    pub size_bytes: u64,
}

/// Why a digest string did not parse.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ParseDigestError {
    /// The string has no `/` between the hash and the size.
    #[error("digest {0:?} is not of the form <hash>/<size>")]
    MissingSeparator(String),
    /// The hash is not exactly 64 characters.
    #[error("digest hash has {0} characters, expected 64")]
    HashLength(usize),
    /// The hash has a character that is not lowercase hexadecimal.
    #[error("digest hash has {0:?} at offset {1}; only 0-9 and a-f are allowed")]
    HashCharacter(char, usize),
    /// The size is not a decimal `u64` (no sign, no leading `+`).
    #[error("digest size {0:?} is not a non-negative decimal integer")]
    Size(String),
}

impl Digest {
    /// Builds a digest from its parts.
    #[must_use]
    pub const fn new(function: DigestFunction, hash: [u8; 32], size_bytes: u64) -> Self {
        Self {
            function,
            hash,
            size_bytes,
        }
    }

    /// Parses `<lowercase hex>/<size>` for the given hash function.
    ///
    /// Uppercase hex is rejected rather than folded, so that one digest has exactly one
    /// text form.
    pub fn parse(function: DigestFunction, s: &str) -> Result<Self, ParseDigestError> {
        let (hex, size) = s
            .split_once('/')
            .ok_or_else(|| ParseDigestError::MissingSeparator(s.to_owned()))?;
        let hash = parse_hex32(hex)?;
        // `u64::from_str` accepts a leading `+`; a digest has one text form, so refuse it.
        if size.is_empty() || !size.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseDigestError::Size(size.to_owned()));
        }
        let size_bytes = size
            .parse()
            .map_err(|_| ParseDigestError::Size(size.to_owned()))?;
        Ok(Self::new(function, hash, size_bytes))
    }

    /// The hash as 64 lowercase hexadecimal characters.
    #[must_use]
    pub fn hash_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(64);
        for byte in self.hash {
            out.push(char::from(DIGITS[usize::from(byte >> 4)]));
            out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
        }
        out
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.hash_hex(), self.size_bytes)
    }
}

fn parse_hex32(hex: &str) -> Result<[u8; 32], ParseDigestError> {
    if hex.len() != 64 {
        return Err(ParseDigestError::HashLength(hex.chars().count()));
    }
    let nibble = |offset: usize, c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        // Report the character, not the byte, so a non-ASCII input reads correctly.
        _ => Err(ParseDigestError::HashCharacter(
            hex[offset..].chars().next().unwrap_or('?'),
            offset,
        )),
    };
    let bytes = hex.as_bytes();
    let mut hash = [0u8; 32];
    for (i, out) in hash.iter_mut().enumerate() {
        let hi = nibble(2 * i, bytes[2 * i])?;
        let lo = nibble(2 * i + 1, bytes[2 * i + 1])?;
        *out = (hi << 4) | lo;
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 of the empty string.
    const EMPTY_HEX: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn digest(size_bytes: u64) -> Digest {
        Digest::parse(DigestFunction::Sha256, &format!("{EMPTY_HEX}/{size_bytes}")).unwrap()
    }

    /// Catches: equality (or ordering) that ignores `size_bytes`, which would let the
    /// store serve a blob of the wrong length for a hash it already holds.
    #[test]
    fn equality_includes_size() {
        assert_eq!(digest(0), digest(0));
        assert_ne!(digest(0), digest(1));
        assert!(digest(0) < digest(1));
    }

    /// Catches: a hex encoder or decoder that swaps nibbles, drops a byte, or emits
    /// uppercase, any of which breaks the round trip through REAPI resource names.
    #[test]
    fn text_form_round_trips() {
        let d = digest(42);
        assert_eq!(d.hash[0], 0xe3);
        assert_eq!(d.hash[31], 0x55);
        assert_eq!(d.to_string(), format!("{EMPTY_HEX}/42"));
        assert_eq!(Digest::parse(DigestFunction::Sha256, &d.to_string()), Ok(d));
    }

    /// Catches: a parser that accepts more than one spelling of the same digest, or
    /// malformed input, which would split one blob into several cache keys.
    #[test]
    fn rejects_non_canonical_text() {
        let parse = |s: &str| Digest::parse(DigestFunction::Sha256, s);
        assert!(matches!(
            parse(EMPTY_HEX),
            Err(ParseDigestError::MissingSeparator(_))
        ));
        assert_eq!(parse("abc/1"), Err(ParseDigestError::HashLength(3)));
        let upper = EMPTY_HEX.to_uppercase();
        assert_eq!(
            parse(&format!("{upper}/1")),
            Err(ParseDigestError::HashCharacter('E', 0))
        );
        let non_ascii = format!("{}é/1", &EMPTY_HEX[..62]);
        assert_eq!(non_ascii.len() - 2, 64);
        assert_eq!(
            parse(&non_ascii),
            Err(ParseDigestError::HashCharacter('é', 62))
        );
        for size in ["", "+1", "-1", "1 ", "18446744073709551616"] {
            assert_eq!(
                parse(&format!("{EMPTY_HEX}/{size}")),
                Err(ParseDigestError::Size(size.to_owned())),
                "size {size:?}"
            );
        }
    }
}
