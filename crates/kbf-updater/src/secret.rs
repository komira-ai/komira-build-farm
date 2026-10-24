//! The secret of the operator API: the bearer token, read from a file that passes the server's
//! own file rule.
//!
//! It is read on every call (so a rotated token needs no restart), held only in the request
//! buffer, and never in an error text, a `Debug` or a log. An error names the flag of the
//! file, not its path: it reaches `GET /rolls/{id}`.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

/// The shortest and longest token, as the server's `--api-token-file` takes it.
const TOKEN_MIN: usize = 32;
const TOKEN_MAX: u64 = 4096;

/// The flag that names the token file, which error texts name instead of its path.
pub const TOKEN_FLAG: &str = "--token-file";

/// Read the token in the file at `path`: a regular file (not followed if it is a symlink,
/// not opened if it is a FIFO) owned by this process's user with mode exactly `0600` or
/// `0400`, one token of visible ASCII, 32 bytes or more, surrounding whitespace dropped. The
/// error text names the flag and the rule, never the path or the content (it reaches
/// `GET /rolls/{id}`).
pub fn read_token(path: &Path) -> Result<String, String> {
    let bytes = read_secret(path, TOKEN_FLAG, TOKEN_MAX)?;
    let fail = |why: &str| format!("{TOKEN_FLAG}: {why}");
    let text = String::from_utf8(bytes)
        .map_err(|_| fail("it must hold one token of 32 to 4096 visible ASCII characters"))?;
    let token = text.trim();
    let visible = token.bytes().all(|b| b.is_ascii_graphic());
    if text.len() as u64 > TOKEN_MAX || token.len() < TOKEN_MIN || !visible {
        return Err(fail(
            "it must hold one token of 32 to 4096 visible ASCII characters",
        ));
    }
    Ok(token.to_owned())
}

/// The bytes of a secret file that passes the file rule, at most `limit` of them (and one
/// more, so that a longer file is seen to be too long). The error text names `flag`.
pub(crate) fn read_secret(path: &Path, flag: &str, limit: u64) -> Result<Vec<u8>, String> {
    let fail = |why: &str| format!("{flag}: {why}");
    let io = |e: std::io::Error| fail(&e.kind().to_string());
    let (file, meta) = open_checked(path).map_err(io)?;
    file_rule(meta.is_file(), meta.uid(), meta.mode(), euid()).map_err(fail)?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).map_err(io)?;
    Ok(bytes)
}

/// The effective user of this process.
pub fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Open without following a symlink and without blocking on a FIFO; the metadata is of the
/// open file, so what is checked is what is read.
fn open_checked(path: &Path) -> std::io::Result<(std::fs::File, std::fs::Metadata)> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    Ok((file, meta))
}

/// The rule for the file itself: regular, owned by `me`, mode exactly `0600` or `0400`.
pub(crate) fn file_rule(is_file: bool, uid: u32, mode: u32, me: u32) -> Result<(), &'static str> {
    if !is_file {
        return Err("it is not a regular file");
    }
    if uid != me {
        return Err("it is not owned by the updater's user");
    }
    if !matches!(mode & 0o7777, 0o600 | 0o400) {
        return Err("its mode must be 0600 or 0400");
    }
    Ok(())
}
