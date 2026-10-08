//! The operator API's write token: the secret a write presents as
//! `Authorization: Bearer <token>` (`docs/api.md`).
//!
//! The token is read once, at start, from the file `--api-token-file` names; it never
//! goes on the command line, and its `Debug` shows no byte of it. The file must be a
//! regular file owned by the server's effective user and readable by that user alone
//! (mode 0600 or 0400): a token others could read authenticates nobody. The file is
//! opened first and its metadata read from the open file, so what is checked is what
//! is read.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use subtle::ConstantTimeEq;

/// The fewest bytes a token may have.
pub const MIN_TOKEN_BYTES: usize = 32;

/// The secret operator API writes must present.
#[derive(Clone)]
pub struct ApiToken(Vec<u8>);

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
    #[error("{path} must hold one token of at least {MIN_TOKEN_BYTES} visible ASCII characters")]
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
        use std::os::unix::fs::MetadataExt;

        let read = |source| TokenFileError::Read {
            path: path.to_owned(),
            source,
        };
        let mut file = std::fs::File::open(path).map_err(read)?;
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
        if mode & !0o600 != 0 {
            return Err(TokenFileError::Mode {
                path: path.to_owned(),
                mode,
            });
        }
        let mut content = Vec::new();
        file.read_to_end(&mut content).map_err(read)?;
        let token = content.trim_ascii();
        if token.len() < MIN_TOKEN_BYTES || !token.iter().all(u8::is_ascii_graphic) {
            return Err(TokenFileError::Content {
                path: path.to_owned(),
            });
        }
        Ok(Self(token.to_vec()))
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
    /// token as `Bearer <token>` (the scheme in any case). The bytes are compared in
    /// constant time; only the length can show through timing.
    #[must_use]
    pub fn admits(&self, authorization: &[u8]) -> bool {
        let Some(space) = authorization.iter().position(|&b| b == b' ') else {
            return false;
        };
        let (scheme, presented) = authorization.split_at(space);
        let presented = presented.trim_ascii_start();
        scheme.eq_ignore_ascii_case(b"bearer") && bool::from(presented.ct_eq(&self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a token shown by `Debug` (it would reach a log through any `{:?}` of a
    /// value holding it).
    #[test]
    fn debug_shows_no_byte_of_the_token() {
        let token = ApiToken(b"s3cr3t".to_vec());
        assert_eq!(format!("{token:?}"), "ApiToken(..)");
    }
}
