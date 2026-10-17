//! The enroll token file (design sections 3.2, 4.1 and 5.1).
//!
//! The file is the credential, so it is read only if nobody else could have written or
//! read it: a regular file (not a symlink, which is refused at `open`, so there is no
//! window between a check and a use), owned by the service user, mode `0600` or `0400`,
//! at most 4096 bytes, one line. A line that is not a token is refused by the node
//! before any network call, naming the part that is wrong and never repeating it.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use kbf_types::{Token, TokenError};

/// The largest token file: a token line is 100 characters; the rest is slack for a
/// trailing newline and a mistake worth reporting.
pub const MAX_TOKEN_FILE_BYTES: u64 = 4096;

/// Why the token file cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum TokenFileError {
    /// The file is not there, or could not be opened or read.
    #[error("token file {path}: {source}")]
    Io {
        /// The file.
        path: String,
        /// The operating system's error.
        source: std::io::Error,
    },
    /// The path is a symlink.
    #[error("token file {0} is a symlink")]
    Symlink(String),
    /// The path is not a regular file.
    #[error("token file {0} is not a regular file")]
    NotRegular(String),
    /// Another user owns it.
    #[error("token file {path} is owned by uid {found}, not {expected}")]
    Owner {
        /// The file.
        path: String,
        /// Its owner.
        found: u32,
        /// The service user.
        expected: u32,
    },
    /// Others could read or write it.
    #[error("token file {path} has mode {mode:04o}; it must be 0600 or 0400")]
    Mode {
        /// The file.
        path: String,
        /// Its permission bits.
        mode: u32,
    },
    /// More than [`MAX_TOKEN_FILE_BYTES`].
    #[error("token file {0} is larger than {MAX_TOKEN_FILE_BYTES} bytes")]
    TooLarge(String),
    /// Empty, or more than one line.
    #[error("token file {0} must hold exactly one line")]
    Lines(String),
    /// The line is not a token. The error names the part, never the text.
    #[error("token file {path}: {source}")]
    Malformed {
        /// The file.
        path: String,
        /// Which part is wrong.
        source: TokenError,
    },
}

/// Reads the token in `path`, which `euid` must own.
///
/// # Errors
/// See [`TokenFileError`].
pub fn read(path: &Path, euid: u32) -> Result<Token, TokenFileError> {
    let name = path.display().to_string();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            Some(libc::ELOOP) => TokenFileError::Symlink(name.clone()),
            _ => io_error(&name)(e),
        })?;
    let meta = file.metadata().map_err(io_error(&name))?;
    if !meta.is_file() {
        return Err(TokenFileError::NotRegular(name));
    }
    if meta.uid() != euid {
        return Err(TokenFileError::Owner {
            path: name,
            found: meta.uid(),
            expected: euid,
        });
    }
    let mode = meta.mode() & 0o777;
    if mode != 0o600 && mode != 0o400 {
        return Err(TokenFileError::Mode { path: name, mode });
    }
    // One byte past the limit is read so that a file over it is refused, whatever its
    // length said when it was opened.
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_TOKEN_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error(&name))?;
    if bytes.len() as u64 > MAX_TOKEN_FILE_BYTES {
        return Err(TokenFileError::TooLarge(name));
    }
    let line = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    if line.is_empty() || line.contains(&b'\n') {
        return Err(TokenFileError::Lines(name));
    }
    // Bytes that are not UTF-8 are not a token either; the error names the part, not them.
    String::from_utf8_lossy(line)
        .parse::<Token>()
        .map_err(|source| TokenFileError::Malformed { path: name, source })
}

fn io_error(path: &str) -> impl Fn(std::io::Error) -> TokenFileError + '_ {
    move |source| TokenFileError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Removes the token file after a successful enrollment. A file already gone is fine.
///
/// # Errors
/// Any other failure to remove it.
pub fn remove(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests;
