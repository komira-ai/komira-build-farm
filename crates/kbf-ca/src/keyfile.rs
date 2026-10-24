//! Files that hold the CA key: created closed, loaded only if still closed.
//!
//! The key file is made with `create_new` and its mode at creation, so there is no moment
//! when it exists and others can read it, and `create_new` (`O_EXCL`) never follows a
//! symbolic link, so a link planted where the key goes makes the write fail instead of
//! landing somewhere else. A load opens with `O_NOFOLLOW`, then checks the opened file, not
//! the path, so what is checked is what is read.

use std::fs::{self, DirBuilder, File, Metadata, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use rustix::fs::OFlags;
use rustix::io::Errno;

use crate::error::KeyFileFault;

fn io_fault(e: io::Error) -> KeyFileFault {
    KeyFileFault::Io(e.kind())
}

/// The account the process runs as: whose files a load accepts.
pub(crate) fn effective_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// Owned by `uid` and closed to group and others (`mode_fault` is what an open mode is).
fn check_private(meta: &Metadata, uid: u32, mode_fault: KeyFileFault) -> Result<(), KeyFileFault> {
    if meta.uid() != uid {
        return Err(KeyFileFault::Owner);
    }
    if meta.mode() & 0o077 != 0 {
        return Err(mode_fault);
    }
    Ok(())
}

/// Makes `dir` (mode 0700) or accepts an existing one that is a real directory, owned by
/// `uid`, closed to group and others.
pub(crate) fn ensure_dir(dir: &Path, uid: u32) -> Result<(), KeyFileFault> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io_fault(e)),
    }
    check_dir(dir, uid)
}

/// Checks that `dir` is a real directory owned by `uid` and closed to group and others.
pub(crate) fn check_dir(dir: &Path, uid: u32) -> Result<(), KeyFileFault> {
    check_dir_mask(dir, uid, 0o077)
}

/// Checks that `dir` is a real directory owned by `uid` that others cannot enter and the
/// group can only read and search (no group write): the shape of a CA directory whose
/// `ca.pem` is shared with a group while `ca.key` stays private to `uid`.
pub(crate) fn check_shared_dir(dir: &Path, uid: u32) -> Result<(), KeyFileFault> {
    check_dir_mask(dir, uid, 0o027)
}

/// A real directory owned by `uid` with none of the mode bits of `forbidden` set.
fn check_dir_mask(dir: &Path, uid: u32, forbidden: u32) -> Result<(), KeyFileFault> {
    let meta = fs::symlink_metadata(dir).map_err(io_fault)?;
    if meta.file_type().is_symlink() {
        return Err(KeyFileFault::Symlink);
    }
    if !meta.is_dir() {
        return Err(KeyFileFault::NotRegular);
    }
    if meta.uid() != uid {
        return Err(KeyFileFault::Owner);
    }
    if meta.mode() & forbidden != 0 {
        return Err(KeyFileFault::DirMode);
    }
    Ok(())
}

/// Writes a file that must not exist, with `mode` from the first byte.
pub(crate) fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<(), KeyFileFault> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(io_fault)?;
    file.write_all(bytes).map_err(io_fault)?;
    file.sync_all().map_err(io_fault)
}

/// Opens for reading without following a symbolic link in the last component.
fn open_nofollow(path: &Path) -> Result<File, KeyFileFault> {
    // The flag is a small positive constant on every platform kbf runs on.
    let flags = OFlags::NOFOLLOW.bits() as i32;
    OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(Errno::LOOP.raw_os_error()) {
                KeyFileFault::Symlink
            } else {
                io_fault(e)
            }
        })
}

/// Reads a secret file: refuses a symlink, anything but a regular file, another owner than
/// `uid`, and any mode bit for group or others.
pub(crate) fn read_secret(path: &Path, uid: u32) -> Result<String, KeyFileFault> {
    let mut file = open_nofollow(path)?;
    let meta = file.metadata().map_err(io_fault)?;
    if !meta.is_file() {
        return Err(KeyFileFault::NotRegular);
    }
    check_private(&meta, uid, KeyFileFault::Mode)?;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(io_fault)?;
    Ok(text)
}

/// Reads a public file (no owner or mode check), refusing a symlink.
pub(crate) fn read_public(path: &Path) -> Result<Vec<u8>, KeyFileFault> {
    let mut bytes = Vec::new();
    open_nofollow(path)?
        .read_to_end(&mut bytes)
        .map_err(io_fault)?;
    Ok(bytes)
}
