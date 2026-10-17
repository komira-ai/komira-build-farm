//! The inbox entry: a directory the unprivileged `kbf` process writes and the root stage
//! reads, so everything in it is an attacker's until it is checked.
//!
//! [`Entry::open`] opens the entry directory without following a link; every file beneath
//! it is opened by name, relative to that open directory, with `O_NOFOLLOW` (and, on
//! Linux, `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`,
//! which refuses a link in any component). A name is a single path component (the offer's
//! file names are checked before they get here), so that is every component.
//!
//! The file is then judged by its open descriptor, never by a path stated before:
//! a symbolic link (ELOOP), a FIFO, socket, device or directory (not a regular file), a
//! hard-linked file (another name for it exists somewhere), an empty one and one over its
//! size cap are refused. A FIFO is opened non-blocking so that opening it cannot hang the
//! stage. Nothing outside the entry is read.
//!
//! The entry's own parent is opened the ordinary way: it is the inbox root, a path root
//! chose, not one the entry's writer did.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rustix::fs::{Dir, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::refusal::{InboxFault, Refusal};

/// The most files an entry may hold; more is refused before any is read.
pub const MAX_FILES: usize = 16;

/// How a file is opened beneath the entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// `openat2` refusing any link and any way out of the entry (Linux 5.6 and later).
    Beneath,
    /// `openat` with `O_NOFOLLOW`: the last component is not followed, which for a single
    /// component name is all of them.
    NoFollow,
}

impl OpenMode {
    /// [`Beneath`](OpenMode::Beneath) when the kernel has `openat2`, else
    /// [`NoFollow`](OpenMode::NoFollow).
    pub fn for_entry(dir: &OwnedFd) -> OpenMode {
        OpenMode::choose(open_beneath(dir, ".").is_ok())
    }

    fn choose(beneath_works: bool) -> OpenMode {
        if beneath_works {
            OpenMode::Beneath
        } else {
            OpenMode::NoFollow
        }
    }
}

/// An open inbox entry directory.
pub struct Entry {
    dir: OwnedFd,
    mode: OpenMode,
    /// How the entry is named in a refusal.
    name: String,
}

impl Entry {
    /// Open `entry_dir`, which must be a directory and not a link.
    pub fn open(entry_dir: &Path) -> Result<Entry, Refusal> {
        let name = entry_dir.display().to_string();
        let refuse = |fault| Refusal::Inbox {
            name: name.clone(),
            fault,
        };
        let (Some(parent), Some(leaf)) = (entry_dir.parent(), entry_dir.file_name()) else {
            return Err(refuse(InboxFault::Missing));
        };
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let parent = rustix::fs::open(parent, flags, Mode::empty())
            .map_err(|e| refuse(fault_of(e, InboxFault::NotADirectory)))?;
        let dir = rustix::fs::openat(&parent, leaf, flags | OFlags::NOFOLLOW, Mode::empty())
            .map_err(|e| refuse(fault_of(e, InboxFault::NotADirectory)))?;
        let mode = OpenMode::for_entry(&dir);
        Ok(Entry { dir, mode, name })
    }

    /// The same entry, opened beneath by `mode` (a test uses both).
    pub fn with_mode(mut self, mode: OpenMode) -> Entry {
        self.mode = mode;
        self
    }

    /// How a refusal names this entry.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The names of the entry's contents, without `.` and `..`; refused when there are
    /// more than [`MAX_FILES`].
    pub fn names(&self) -> Result<Vec<String>, Refusal> {
        let refuse = |fault| Refusal::Inbox {
            name: self.name.clone(),
            fault,
        };
        let dir = Dir::read_from(&self.dir)
            .map_err(unreadable)
            .map_err(&refuse)?;
        let mut names = Vec::new();
        for item in dir {
            let item = item.map_err(unreadable).map_err(&refuse)?;
            let name = item.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            names.push(String::from_utf8_lossy(name).into_owned());
            if names.len() > MAX_FILES {
                return Err(refuse(InboxFault::TooMany { max: MAX_FILES }));
            }
        }
        Ok(names)
    }

    /// The directory `name` inside this entry, opened relative to it without following a
    /// link, so a link put in the writer's place of `name` is refused instead of followed
    /// out of the entry.
    pub fn sub(&self, name: &str) -> Result<Entry, Refusal> {
        let refuse = |fault| Refusal::Inbox {
            name: name.to_owned(),
            fault,
        };
        if matches!(name, "" | "." | "..") || name.contains(['/', '\0']) {
            return Err(refuse(InboxFault::BadName));
        }
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let dir = rustix::fs::openat(&self.dir, name, flags, Mode::empty())
            .map_err(|e| refuse(fault_of(e, InboxFault::NotADirectory)))?;
        Ok(Entry {
            dir,
            mode: self.mode,
            name: format!("{}/{name}", self.name),
        })
    }

    /// Remove the file `name` from the entry, by `unlinkat` on the entry's descriptor: the
    /// name is one component of the directory that was opened, so no link in the writer's
    /// control can send the removal elsewhere (it removes a link itself, never its target).
    /// A name that is not there is removed already.
    pub fn remove_file(&self, name: &str) -> Result<(), Refusal> {
        let refuse = |fault| Refusal::Inbox {
            name: name.to_owned(),
            fault,
        };
        if matches!(name, "" | "." | "..") || name.contains(['/', '\0']) {
            return Err(refuse(InboxFault::BadName));
        }
        match rustix::fs::unlinkat(&self.dir, name, rustix::fs::AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(refuse(fault_of(e, InboxFault::NotRegular))),
        }
    }

    /// Open the file `name` beneath the entry and judge it by its descriptor: a regular
    /// file with one link, not empty, of at most `cap` bytes.
    pub fn open_file(&self, name: &str, cap: u64) -> Result<File, Refusal> {
        let refuse = |fault| Refusal::Inbox {
            name: name.to_owned(),
            fault,
        };
        // One path component: `NoFollow` would follow a `..` or a `/` out of the entry.
        if matches!(name, "" | "." | "..") || name.contains(['/', '\0']) {
            return Err(refuse(InboxFault::BadName));
        }
        let fd = match self.mode {
            OpenMode::Beneath => open_beneath(&self.dir, name),
            OpenMode::NoFollow => open_nofollow(&self.dir, name),
        }
        .map_err(|e| refuse(fault_of(e, InboxFault::NotRegular)))?;
        let stat = rustix::fs::fstat(&fd)
            .map_err(unreadable)
            .map_err(&refuse)?;
        // `st_nlink` is a u64 on x86-64 and a u16 on macOS.
        #[allow(clippy::useless_conversion)]
        let nlink = u64::from(stat.st_nlink);
        let fault = verdict(
            FileType::from_raw_mode(stat.st_mode),
            nlink,
            u64::try_from(stat.st_size).unwrap_or(0),
            cap,
        );
        match fault {
            Some(fault) => Err(refuse(fault)),
            None => Ok(File::from(fd)),
        }
    }
}

/// What an open file's type, link count and size say against the rules, if anything.
fn verdict(kind: FileType, links: u64, size: u64, cap: u64) -> Option<InboxFault> {
    if kind != FileType::RegularFile {
        return Some(InboxFault::NotRegular);
    }
    if links > 1 {
        return Some(InboxFault::HardLinked);
    }
    if size == 0 {
        return Some(InboxFault::Empty);
    }
    if size > cap {
        return Some(InboxFault::Oversize { cap });
    }
    None
}

/// The refusal an `open` error means: a link (ELOOP), a missing name, a name that is
/// not the kind of thing wanted (ENOTDIR: `wrong` says what), or an error.
fn fault_of(errno: Errno, wrong: InboxFault) -> InboxFault {
    match errno {
        Errno::LOOP => InboxFault::Symlink,
        Errno::NOENT => InboxFault::Missing,
        Errno::NOTDIR => wrong,
        other => unreadable(other),
    }
}

/// An error of reading the entry that no rule names.
fn unreadable(errno: Errno) -> InboxFault {
    InboxFault::Unreadable(io::Error::from(errno))
}

fn open_nofollow(dir: &OwnedFd, name: &str) -> rustix::io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    rustix::fs::openat(dir, name, flags, Mode::empty())
}

#[cfg(target_os = "linux")]
fn open_beneath(dir: &OwnedFd, name: &str) -> rustix::io::Result<OwnedFd> {
    use rustix::fs::ResolveFlags;
    let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
    rustix::fs::openat2(dir, name, flags, Mode::empty(), resolve)
}

/// Where `openat2` does not exist, [`OpenMode::for_entry`] finds it fails.
#[cfg(not(target_os = "linux"))]
fn open_beneath(_dir: &OwnedFd, _name: &str) -> rustix::io::Result<OwnedFd> {
    Err(Errno::NOSYS)
}

/// Write the whole of `from` (at most `cap` bytes) to a new file at `to`, mode 0644, and
/// sync it. A source with more than `cap` bytes (it grew since its size was read) is
/// [`InboxFault::Oversize`].
pub fn copy_capped(from: &mut File, to: &Path, cap: u64, name: &str) -> Result<(), Refusal> {
    let work = |source| Refusal::Work {
        path: to.to_owned(),
        source,
    };
    let mut copy = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(to)
        .map_err(work)?;
    let copied = io::copy(&mut from.take(cap + 1), &mut copy).map_err(|e| Refusal::Inbox {
        name: name.to_owned(),
        fault: InboxFault::Unreadable(e),
    })?;
    if copied > cap {
        return Err(Refusal::Inbox {
            name: name.to_owned(),
            fault: InboxFault::Oversize { cap },
        });
    }
    copy.flush().map_err(work)?;
    copy.sync_all().map_err(work)
}

/// The bytes of `from`, at most `cap` of them.
pub fn read_capped(from: &mut File, cap: u64, name: &str) -> Result<Vec<u8>, Refusal> {
    let mut bytes = Vec::new();
    from.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Refusal::Inbox {
            name: name.to_owned(),
            fault: InboxFault::Unreadable(e),
        })?;
    if bytes.len() as u64 > cap {
        return Err(Refusal::Inbox {
            name: name.to_owned(),
            fault: InboxFault::Oversize { cap },
        });
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod tests;
