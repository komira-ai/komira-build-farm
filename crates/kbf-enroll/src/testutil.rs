//! Helpers the unit tests share.

use std::path::PathBuf;

use kbf_types::{CaPin, Token, TokenId, TokenSecret};

/// A fresh scratch directory under the system temp dir, resolved through symlinks.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kbf-enroll-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::canonicalize(&dir).expect("canonical scratch dir")
}

/// A token with distinguishable parts.
pub fn token() -> Token {
    Token::new(
        TokenId::from_bits(0x0123_4567_89ab_cdef),
        TokenSecret::from_bytes([7; 32]),
        CaPin::from_bytes([9; 32]),
    )
}
