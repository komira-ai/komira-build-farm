//! The filesystem the store writes through: one directory of flat files.
//!
//! The store never touches the disk except through [`Fs`] and [`FsFile`], so a test can
//! swap in [`FaultFs`](crate::FaultFs), which fails or crashes at any chosen operation
//! and then shows what a power cut would have left on disk. [`StdFs`] is the real one.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// One directory the store owns. Names are plain file names, never paths.
///
/// What is durable follows POSIX: bytes appended to a file are durable once
/// [`FsFile::sync`] on that same handle returns `Ok`, and a created, renamed or removed
/// name is durable once [`Fs::sync_dir`] returns `Ok`.
pub trait Fs {
    /// An open file.
    type File: FsFile;

    /// The names in the directory, in any order.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the listing.
    fn list(&self) -> io::Result<Vec<String>>;

    /// The whole content of `name`.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the read.
    fn read(&self, name: &str) -> io::Result<Vec<u8>>;

    /// Creates `name`, which must not exist, and opens it for appending.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the create, including `name` existing.
    fn create(&self, name: &str) -> io::Result<Self::File>;

    /// Opens the existing `name` for appending.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the open.
    fn open_append(&self, name: &str) -> io::Result<Self::File>;

    /// Renames `from` to `to`, replacing `to` if it exists.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the rename.
    fn rename(&self, from: &str, to: &str) -> io::Result<()>;

    /// Removes `name`.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the removal.
    fn remove(&self, name: &str) -> io::Result<()>;

    /// Makes every create, rename and removal so far durable.
    ///
    /// # Errors
    ///
    /// The I/O error the directory sync returned.
    fn sync_dir(&self) -> io::Result<()>;
}

/// A file opened for appending.
pub trait FsFile {
    /// Appends all of `bytes`.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the write.
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;

    /// Cuts the file to `len` bytes.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped the truncation.
    fn truncate(&mut self, len: u64) -> io::Result<()>;

    /// Makes the file's content and length durable.
    ///
    /// # Errors
    ///
    /// The I/O error the sync returned. After one, what reached the disk is unknown.
    fn sync(&mut self) -> io::Result<()>;
}

/// The real filesystem: one directory on the local disk.
#[derive(Debug)]
pub struct StdFs {
    dir: PathBuf,
}

impl StdFs {
    /// The directory `dir`, created (with its parents) if missing.
    ///
    /// # Errors
    ///
    /// The I/O error that stopped creating the directory.
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Fs for StdFs {
    type File = StdFile;

    fn list(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let name = entry?.file_name();
            names.push(name.into_string().map_err(|n| {
                io::Error::new(io::ErrorKind::InvalidData, format!("file name {n:?}"))
            })?);
        }
        Ok(names)
    }

    fn read(&self, name: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.path(name))
    }

    fn create(&self, name: &str) -> io::Result<StdFile> {
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(self.path(name))?;
        Ok(StdFile(file))
    }

    fn open_append(&self, name: &str) -> io::Result<StdFile> {
        let file = OpenOptions::new().append(true).open(self.path(name))?;
        Ok(StdFile(file))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        std::fs::rename(self.path(from), self.path(to))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        std::fs::remove_file(self.path(name))
    }

    fn sync_dir(&self) -> io::Result<()> {
        File::open(&self.dir)?.sync_all()
    }
}

/// A file of [`StdFs`]. Every sync goes through the handle that wrote, so a write-back
/// error on it is reported to this handle.
#[derive(Debug)]
pub struct StdFile(File);

impl FsFile for StdFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(bytes)
    }

    fn truncate(&mut self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.0.sync_data()
    }
}
