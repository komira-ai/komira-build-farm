//! The operator API's write token: the secret a write presents as
//! `Authorization: Bearer <token>` (`docs/api.md`).
//!
//! The token is read once, at start, from the file `--api-token-file` names; it never
//! goes on the command line, and its `Debug` shows no byte of it. The file must be a
//! regular file owned by the server's effective user and readable by that user alone
//! (mode 0600 or 0400): a token others could read authenticates nobody. The file is
//! opened without blocking (a FIFO is refused at once rather than waited on) and its
//! metadata read from the open file, so what is checked is what is read. At most
//! [`MAX_TOKEN_FILE_BYTES`] are read.
//!
//! Only a SHA-256 digest of the token is kept. A presented token is hashed too and
//! the two digests compared in constant time, so neither the bytes nor the length of
//! the token show through timing.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq;

/// The fewest bytes a token may have.
pub const MIN_TOKEN_BYTES: usize = 32;

/// The most bytes a token file may hold, surrounding whitespace included.
pub const MAX_TOKEN_FILE_BYTES: usize = 4096;

/// The secret operator API writes must present, kept as its SHA-256 digest.
#[derive(Clone)]
pub struct ApiToken([u8; 32]);

impl fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiToken(..)")
    }
}

/// Why a token file is refused.
#[derive(Debug, thiserror::Error)]
pub enum TokenFileError {
    /// It cannot be opened or read.
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// It is not a regular file.
    #[error("{path} is not a regular file")]
    NotAFile { path: PathBuf },
    /// Another user owns it.
    #[error("{path} is owned by uid {owner}, not by this server's uid {server}")]
    Owner {
        path: PathBuf,
        owner: u32,
        server: u32,
    },
    /// Its mode lets someone other than its owner read or write it, or is more than
    /// read and write.
    #[error("{path} has mode {mode:04o}; it must be 0600 or 0400")]
    Mode { path: PathBuf, mode: u32 },
    /// Its content is not a usable token.
    #[error(
        "{path} must hold one token of at least {MIN_TOKEN_BYTES} visible ASCII characters, \
         in at most {MAX_TOKEN_FILE_BYTES} bytes"
    )]
    Content { path: PathBuf },
}

impl ApiToken {
    /// The token in the file at `path`: its content without leading or trailing
    /// whitespace (a trailing newline is usual).
    ///
    /// # Errors
    /// The file cannot be read, is not a regular file, is not owned by this process's
    /// effective user, has a mode other than 0600 or 0400, or does not hold at least
    /// [`MIN_TOKEN_BYTES`] visible ASCII characters (and nothing else but surrounding
    /// whitespace).
    #[cfg(unix)]
    pub fn from_file(path: &Path) -> Result<Self, TokenFileError> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        let read = |source| TokenFileError::Read {
            path: path.to_owned(),
            source,
        };
        // Non-blocking: opening a FIFO for reading would otherwise wait for a writer.
        let nonblocking = rustix::fs::OFlags::NONBLOCK.bits().cast_signed();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nonblocking)
            .open(path)
            .map_err(read)?;
        let meta = file.metadata().map_err(read)?;
        if !meta.is_file() {
            return Err(TokenFileError::NotAFile {
                path: path.to_owned(),
            });
        }
        let server = rustix::process::geteuid().as_raw();
        if meta.uid() != server {
            return Err(TokenFileError::Owner {
                path: path.to_owned(),
                owner: meta.uid(),
                server,
            });
        }
        let mode = meta.mode() & 0o7777;
        if !owner_only(mode) {
            return Err(TokenFileError::Mode {
                path: path.to_owned(),
                mode,
            });
        }
        let mut content = Vec::new();
        let limit = u64::try_from(MAX_TOKEN_FILE_BYTES + 1).unwrap_or(u64::MAX);
        file.take(limit).read_to_end(&mut content).map_err(read)?;
        let token = content.trim_ascii();
        if content.len() > MAX_TOKEN_FILE_BYTES
            || token.len() < MIN_TOKEN_BYTES
            || !token.iter().all(u8::is_ascii_graphic)
        {
            return Err(TokenFileError::Content {
                path: path.to_owned(),
            });
        }
        Ok(Self(Sha256::digest(token).into()))
    }

    /// Elsewhere than Unix the file's owner and mode cannot be checked, so no token
    /// file is accepted.
    ///
    /// # Errors
    /// Always.
    #[cfg(not(unix))]
    pub fn from_file(path: &Path) -> Result<Self, TokenFileError> {
        Err(TokenFileError::Read {
            path: path.to_owned(),
            source: std::io::ErrorKind::Unsupported.into(),
        })
    }

    /// Whether `authorization`, an `Authorization` header's value, presents this
    /// token as `Bearer <token>` (the scheme in any case). The digests of the two are
    /// compared in constant time.
    #[must_use]
    pub fn admits(&self, authorization: &[u8]) -> bool {
        let Some(space) = authorization.iter().position(|&b| b == b' ') else {
            return false;
        };
        let (scheme, presented) = authorization.split_at(space);
        let presented = presented.trim_ascii_start();
        let presented: [u8; 32] = Sha256::digest(presented).into();
        scheme.eq_ignore_ascii_case(b"bearer") && bool::from(presented.ct_eq(&self.0))
    }
}

/// Whether a file mode (permission bits) is 0600 or 0400: readable by its owner, and
/// by no one else, with nothing more.
const fn owner_only(mode: u32) -> bool {
    mode == 0o600 || mode == 0o400
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a token shown by `Debug` (it would reach a log through any `{:?}` of a
    /// value holding it).
    #[test]
    fn debug_shows_no_byte_of_the_token() {
        let token = ApiToken(Sha256::digest(b"s3cr3t").into());
        assert_eq!(format!("{token:?}"), "ApiToken(..)");
        assert!(token.admits(b"Bearer s3cr3t"));
    }

    /// Catches: a mode check looser than the documented 0600 or 0400 (write-only, no
    /// access, owner-executable, setuid, or group and other bits let through).
    #[test]
    fn only_0600_and_0400_are_owner_only() {
        for mode in 0..=0o7777 {
            assert_eq!(
                owner_only(mode),
                mode == 0o600 || mode == 0o400,
                "{mode:04o}"
            );
        }
        for mode in [0o200, 0o000, 0o700, 0o644, 0o4600] {
            assert!(!owner_only(mode), "{mode:04o}");
        }
    }
}
