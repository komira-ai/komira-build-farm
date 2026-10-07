//! Reading an action's outputs without following a symlink at any level, within
//! limits.
//!
//! The directory an action ran in is the action's to shape. It can replace its working
//! directory, or any directory on an output's path, with a symlink to a host path
//! (`rm -rf d; ln -s /host/dir d`, output `d/key.pem`). So no output is read by path.
//! The root is opened once, and every component below it, the working directory's
//! included, is opened relative to its parent's descriptor with `O_NOFOLLOW`. A
//! component on the way that is absent, a file or a symlink means the action did not
//! create the output there, and the output is left out. The last component is examined
//! with `AT_SYMLINK_NOFOLLOW`: a symlink is recorded as one, never dereferenced, and a
//! file is opened with `O_NOFOLLOW` and its type checked again on the descriptor (an
//! entry that changed is an error). The same holds inside an output directory.
//!
//! An output directory is walked iteratively, its stack of frames a `Vec` on the heap,
//! so no depth of tree can overflow a thread's stack. Only the directory being read
//! holds a descriptor: the walk returns to a parent through the child's `..`, and
//! refuses a `..` that is not the directory it came from (device and inode). The walk
//! stops with [`OutputsError::Limit`] once the outputs pass any of the
//! [`OutputLimits`].
//!
//! A file of at most [`CHUNK_BYTES`] is read whole. A larger one is read once in
//! chunks to hash it (stopping as soon as it passes the byte limit), then handed to
//! [`Store::put_file`] from its start, so no output is ever held whole in memory.
//!
//! Each system call runs on tokio's blocking pool.

use std::ffi::OsStr;
use std::io::{Read, Seek};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kbf_proto::reapi::{
    ActionResult, Digest, Directory, DirectoryNode, FileNode, OutputDirectory, OutputFile,
    OutputSymlink, SymlinkNode, Tree,
};
use prost::Message;
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use sha2::{Digest as _, Sha256};

use crate::limits::{Exceeded, OutputLimits};
use crate::store::{CHUNK_BYTES, Store, StoreError, digest_of};

/// Why the outputs could not be read.
#[derive(Debug, thiserror::Error)]
pub enum OutputsError {
    /// An output path, or a name the action left, breaks a rule: the action's error.
    #[error("{0}")]
    Invalid(String),
    /// The action's outputs pass one of the [`OutputLimits`].
    #[error("{path}: the action's outputs exceed the limit on {what} ({limit})")]
    Limit {
        path: PathBuf,
        what: Exceeded,
        limit: u64,
    },
    /// The store refused or failed a blob.
    #[error("{path}: store: {source}")]
    Store {
        path: PathBuf,
        #[source]
        source: StoreError,
    },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Checks an output path or working directory: relative, made of non-empty
/// components other than `.` and `..`, joined by single slashes. The empty path is
/// the directory itself.
fn check_relative(what: &str, path: &str) -> Result<(), OutputsError> {
    let plain = path.is_empty()
        || path
            .split('/')
            .all(|part| !matches!(part, "" | "." | "..") && !part.contains('\0'));
    if !plain {
        return Err(OutputsError::Invalid(format!(
            "{what} {path:?} must be relative and must not contain . or .."
        )));
    }
    Ok(())
}

/// What one action's outputs have used of its [`OutputLimits`] so far.
struct Budget {
    limits: OutputLimits,
    entries: u64,
    bytes: u64,
}

impl Budget {
    fn entries_left(&self) -> u64 {
        self.limits.max_entries.saturating_sub(self.entries)
    }

    fn bytes_left(&self) -> u64 {
        self.limits.max_bytes.saturating_sub(self.bytes)
    }

    /// Counts `n` entries found at `shown`.
    fn entries(&mut self, n: u64, shown: &Path) -> Result<(), OutputsError> {
        if n > self.entries_left() {
            return Err(limit(shown, Exceeded::Entries, self.limits.max_entries));
        }
        self.entries += n;
        Ok(())
    }

    /// Counts the bytes of the file read at `shown`; `None` is one that did not fit.
    fn file(
        &mut self,
        read: Option<(Blob, bool)>,
        shown: &Path,
    ) -> Result<(Blob, bool), OutputsError> {
        let Some((blob, executable)) = read else {
            return Err(limit(shown, Exceeded::Bytes, self.limits.max_bytes));
        };
        self.bytes += blob.size();
        Ok((blob, executable))
    }
}

fn limit(shown: &Path, what: Exceeded, limit: u64) -> OutputsError {
    OutputsError::Limit {
        path: shown.to_owned(),
        what,
        limit,
    }
}

/// Opens a directory, refusing a symlink in its place.
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Opens a regular file for reading, refusing a symlink, and never blocking on a FIFO
/// or claiming a terminal that replaced it after the `stat`.
const FILE: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOCTTY)
    .union(OFlags::CLOEXEC);

/// What a name in a directory is, read without following it.
enum Kind {
    File {
        executable: bool,
    },
    Directory,
    Symlink,
    /// A socket, FIFO or device: left out.
    Other,
}

#[allow(clippy::unnecessary_cast)] // `st_mode` is u32 on Linux, u16 on macOS
fn kind(stat: &Stat) -> Kind {
    match FileType::from_raw_mode(stat.st_mode as _) {
        FileType::RegularFile => Kind::File {
            executable: stat.st_mode & 0o111 != 0,
        },
        FileType::Directory => Kind::Directory,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    }
}

/// A file's bytes, as they are stored.
#[derive(Debug)]
enum Blob {
    /// At most [`CHUNK_BYTES`], read whole.
    Bytes(Vec<u8>),
    /// Larger: the open file, at its start, and what its bytes hash to.
    File(std::fs::File, Digest),
}

impl Blob {
    fn size(&self) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::File(_, digest) => digest.size_bytes.unsigned_abs(),
        }
    }

    async fn store(self, store: &impl Store, shown: &Path) -> Result<Digest, OutputsError> {
        let stored = match self {
            Self::Bytes(bytes) => store.put(bytes).await,
            Self::File(file, digest) => store.put_file(file, digest).await,
        };
        stored.map_err(|source| OutputsError::Store {
            path: shown.to_owned(),
            source,
        })
    }
}

/// Runs `f` on the blocking pool; a system call error is reported against `shown`
/// (a path for the message only, never opened).
async fn blocking<T: Send + 'static>(
    shown: &Path,
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, OutputsError> {
    let io = |source: std::io::Error| OutputsError::Io {
        path: shown.to_owned(),
        source,
    };
    // A task that panicked (a bug here) or was cancelled (the runtime shutting down)
    // is an I/O error like the call's own.
    tokio::task::spawn_blocking(f)
        .await
        .map_err(std::io::Error::other)
        .and_then(|done| done)
        .map_err(io)
}

/// Opens each of `parts` below `start` as a directory. `None` when one is absent, a
/// file or a symlink.
fn descend(start: &OwnedFd, parts: &[String]) -> std::io::Result<Option<OwnedFd>> {
    let mut here = start.as_fd().try_clone_to_owned()?;
    for part in parts {
        here = match rustix::fs::openat(&here, part.as_str(), DIRECTORY, Mode::empty()) {
            Ok(fd) => fd,
            // ELOOP: a symlink stands there; ENOTDIR: a file (or, on some kernels, a
            // symlink under O_DIRECTORY).
            Err(Errno::NOENT | Errno::NOTDIR | Errno::LOOP) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
    }
    Ok(Some(here))
}

/// What `name` in `dir` is, or `None` when it is absent.
fn examine(dir: &OwnedFd, name: &OsStr) -> std::io::Result<Option<Kind>> {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(kind(&stat))),
        Err(Errno::NOENT) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The regular file `name` in `dir` as a [`Blob`], with its executable bit, or `None`
/// when it holds more than `max` bytes (at most `max + 1` are read). The type is
/// checked again on the open descriptor: an entry that stopped being a regular file
/// after it was examined is an error, never read.
fn read_file(dir: &OwnedFd, name: &OsStr, max: u64) -> std::io::Result<Option<(Blob, bool)>> {
    let fd = rustix::fs::openat(dir, name, FILE, Mode::empty())?;
    let Kind::File { executable } = kind(&rustix::fs::fstat(&fd)?) else {
        return Err(std::io::Error::other(
            "is no longer a regular file once opened",
        ));
    };
    // The read itself is bounded, not trusted to `st_size`: a file can grow.
    let mut file = std::fs::File::from(fd);
    let mut head = Vec::new();
    (&mut file)
        .take((CHUNK_BYTES as u64).min(max.saturating_add(1)))
        .read_to_end(&mut head)?;
    if head.len() as u64 > max {
        return Ok(None);
    }
    if head.len() < CHUNK_BYTES {
        return Ok(Some((Blob::Bytes(head), executable)));
    }
    // A full first chunk: hash the rest in chunks, then store the file from its start.
    let mut hasher = Sha256::new();
    hasher.update(&head);
    let mut size = head.len() as u64;
    let mut chunk = head;
    loop {
        chunk.clear();
        (&mut file)
            .take((CHUNK_BYTES as u64).min(max.saturating_add(1) - size))
            .read_to_end(&mut chunk)?;
        if chunk.is_empty() {
            break;
        }
        size += chunk.len() as u64;
        if size > max {
            return Ok(None);
        }
        hasher.update(&chunk);
    }
    file.rewind()?;
    let digest = Digest {
        hash: hex::encode(hasher.finalize()),
        size_bytes: i64::try_from(size).map_err(std::io::Error::other)?,
    };
    Ok(Some((Blob::File(file, digest), executable)))
}

/// `raw` as a REAPI name, or the action's error naming `shown`.
fn utf8(raw: Vec<u8>, shown: &Path) -> Result<String, OutputsError> {
    String::from_utf8(raw).map_err(|_| {
        OutputsError::Invalid(format!(
            "{}: an output name or symlink target is not UTF-8",
            shown.display()
        ))
    })
}

/// The target of the symlink `name` in `dir`, as written; never followed.
fn read_link(dir: &OwnedFd, name: &OsStr) -> std::io::Result<Vec<u8>> {
    Ok(rustix::fs::readlinkat(dir, name, Vec::new())?.into_bytes())
}

/// An entry of a directory being walked: its name and what it is.
type Entry = (Vec<u8>, Kind);

/// The entries of `dir` (without `.` and `..`), sorted by name as REAPI wants, each
/// with what it is. Stops after `max + 1` entries, so a directory past the entry limit
/// is never read whole; the caller refuses it.
fn list(dir: &OwnedFd, max: u64) -> std::io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in Dir::read_from(dir)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name == b"." || name == b".." {
            continue;
        }
        let stat = rustix::fs::statat(dir, OsStr::from_bytes(&name), AtFlags::SYMLINK_NOFOLLOW)?;
        entries.push((name, kind(&stat)));
        if entries.len() as u64 > max {
            break;
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// A directory's device and inode.
type Identity = (u64, u64);

#[allow(clippy::unnecessary_cast)] // the field types differ between Linux and macOS
fn identity(fd: &OwnedFd) -> std::io::Result<Identity> {
    let stat = rustix::fs::fstat(fd)?;
    Ok((stat.st_dev as u64, stat.st_ino as u64))
}

/// Opens the directory `name` in `dir`: its descriptor, identity and entries (at most
/// `max + 1`, see [`list`]).
fn open_listed(
    dir: &OwnedFd,
    name: &OsStr,
    max: u64,
) -> std::io::Result<(OwnedFd, Identity, Vec<Entry>)> {
    let fd = rustix::fs::openat(dir, name, DIRECTORY, Mode::empty())?;
    let id = identity(&fd)?;
    let entries = list(&fd, max)?;
    Ok((fd, id, entries))
}

/// Opens the parent of `dir` through its `..`, refusing any directory but `expected`:
/// the walk goes back only where it came from.
fn back_to_parent(dir: &OwnedFd, expected: Identity) -> std::io::Result<OwnedFd> {
    let fd = rustix::fs::openat(dir, "..", DIRECTORY, Mode::empty())?;
    if identity(&fd)? != expected {
        return Err(std::io::Error::other(
            "the walk's parent directory was replaced",
        ));
    }
    Ok(fd)
}

/// Reads each output path under `working_directory` in `root` into `store` and
/// records it in `result`, all of them together within `limits`. `root` is the
/// driver's own directory and is opened as given; nothing below it is followed. A
/// path the action did not create is left out, as is one under a directory the action
/// replaced with a file or a symlink, and an entry that is not a file, directory or
/// symlink. Each path in `result` is relative to the working directory.
///
/// # Errors
/// [`OutputsError::Invalid`] for a bad path or a name that is not UTF-8;
/// [`OutputsError::Limit`] past a limit; a failed read or store.
pub async fn collect(
    store: &impl Store,
    root: &Path,
    working_directory: &str,
    paths: &[String],
    limits: OutputLimits,
    result: &mut ActionResult,
) -> Result<(), OutputsError> {
    check_relative("working directory", working_directory)?;
    let base = root.join(working_directory);
    let root_fd = {
        let root = root.to_owned();
        Arc::new(
            blocking(&base, move || {
                Ok(rustix::fs::openat(CWD, &root, DIRECTORY, Mode::empty())?)
            })
            .await?,
        )
    };
    let wd_parts: Vec<String> = working_directory
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    let mut budget = Budget {
        limits,
        entries: 0,
        bytes: 0,
    };
    for path in paths {
        if path.is_empty() {
            return Err(OutputsError::Invalid("an output path is empty".to_owned()));
        }
        check_relative("output path", path)?;
        let shown = base.join(path);
        let (dirs, name) = path.rsplit_once('/').unwrap_or(("", path));
        let mut parts: Vec<String> = wd_parts.clone();
        parts.extend(dirs.split('/').filter(|p| !p.is_empty()).map(str::to_owned));
        let name = PathBuf::from(name);
        let start = Arc::clone(&root_fd);
        // Absent, a file or a symlink on the way: the action did not create the output.
        let Some(parent) = blocking(&shown, move || descend(&start, &parts)).await? else {
            continue;
        };
        let parent = Arc::new(parent);
        let found = {
            let (parent, name) = (Arc::clone(&parent), name.clone());
            blocking(&shown, move || examine(&parent, name.as_os_str())).await?
        };
        match found {
            None | Some(Kind::Other) => {}
            Some(Kind::File { .. }) => {
                budget.entries(1, &shown)?;
                let max = budget.bytes_left();
                let read =
                    blocking(&shown, move || read_file(&parent, name.as_os_str(), max)).await?;
                let (blob, executable) = budget.file(read, &shown)?;
                result.output_files.push(OutputFile {
                    path: path.clone(),
                    digest: Some(blob.store(store, &shown).await?),
                    is_executable: executable,
                    ..OutputFile::default()
                });
            }
            Some(Kind::Directory) => {
                budget.entries(1, &shown)?;
                let (root, children) =
                    walk(store, &mut budget, parent, name, shown.clone()).await?;
                let root_digest = digest_of(&root.encode_to_vec());
                let tree = Tree {
                    root: Some(root),
                    children,
                };
                let tree_digest = Blob::Bytes(tree.encode_to_vec())
                    .store(store, &shown)
                    .await?;
                result.output_directories.push(OutputDirectory {
                    path: path.clone(),
                    tree_digest: Some(tree_digest),
                    is_topologically_sorted: false,
                    root_directory_digest: Some(root_digest),
                });
            }
            Some(Kind::Symlink) => {
                budget.entries(1, &shown)?;
                let raw = blocking(&shown, move || read_link(&parent, name.as_os_str())).await?;
                result.output_symlinks.push(OutputSymlink {
                    path: path.clone(),
                    target: utf8(raw, &shown)?,
                    ..OutputSymlink::default()
                });
            }
        }
    }
    Ok(())
}

/// Stores the regular file at `path` (a file the driver made, such as captured
/// stdout; its last component is not followed) and returns its digest: read whole if
/// it is at most [`CHUNK_BYTES`], hashed in chunks and handed to [`Store::put_file`]
/// if larger. A file of more than `max` bytes fails with [`OutputsError::Limit`]
/// ([`Exceeded::Stdio`]) once `max + 1` bytes are read.
///
/// # Errors
/// The file cannot be opened or read, is not a regular file, holds more than `max`
/// bytes, or the store fails.
pub async fn store_file(store: &impl Store, path: &Path, max: u64) -> Result<Digest, OutputsError> {
    let (dir, name) = match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => (dir.to_owned(), name.to_owned()),
        _ => {
            return Err(OutputsError::Invalid(format!(
                "{} names no file",
                path.display()
            )));
        }
    };
    let read = blocking(path, move || {
        let dir = rustix::fs::openat(CWD, &dir, DIRECTORY, Mode::empty())?;
        read_file(&dir, &name, max)
    })
    .await?;
    let (blob, _) = read.ok_or_else(|| limit(path, Exceeded::Stdio, max))?;
    blob.store(store, path).await
}

/// A directory on the walk's stack: what is left of its entries and what is built of
/// its Directory.
struct Frame {
    /// The path shown in errors; never opened.
    shown: PathBuf,
    /// Its name in its parent's Directory.
    name: String,
    /// Checked when the walk comes back to it from a child.
    id: Identity,
    entries: std::vec::IntoIter<Entry>,
    directory: Directory,
    /// Its place in the Tree's `children` (unused for the root).
    slot: usize,
}

/// Opens and lists the directory `name` in `parent`, counting its entries against the
/// budget.
async fn open_frame(
    budget: &mut Budget,
    parent: Arc<OwnedFd>,
    name: PathBuf,
    shown: PathBuf,
    node_name: String,
    slot: usize,
) -> Result<(Arc<OwnedFd>, Frame), OutputsError> {
    let max = budget.entries_left();
    let (fd, id, entries) =
        blocking(&shown, move || open_listed(&parent, name.as_os_str(), max)).await?;
    budget.entries(entries.len() as u64, &shown)?;
    let frame = Frame {
        shown,
        name: node_name,
        id,
        entries: entries.into_iter(),
        directory: Directory::default(),
        slot,
    };
    Ok((Arc::new(fd), frame))
}

/// The directory `name` in `parent` (shown as `shown`) as a Directory, with every
/// Directory below it in pre-order, storing each file. Symlinks inside are recorded,
/// never followed. Iterative, not recursive: see the module comment.
async fn walk(
    store: &impl Store,
    budget: &mut Budget,
    parent: Arc<OwnedFd>,
    name: PathBuf,
    shown: PathBuf,
) -> Result<(Directory, Vec<Directory>), OutputsError> {
    let (mut here, mut root) = open_frame(budget, parent, name, shown, String::new(), 0).await?;
    // The frames below the root; the last is the directory `here` holds open.
    let mut stack: Vec<Frame> = Vec::new();
    let mut children: Vec<Directory> = Vec::new();
    loop {
        let top = stack.last_mut().unwrap_or(&mut root);
        let Some((raw, kind)) = top.entries.next() else {
            // The top directory is done: the walk is, or goes back to its parent.
            let Some(done) = stack.pop() else {
                return Ok((root.directory, children));
            };
            let top = stack.last_mut().unwrap_or(&mut root);
            top.directory.directories.push(DirectoryNode {
                name: done.name,
                digest: Some(digest_of(&done.directory.encode_to_vec())),
            });
            children[done.slot] = done.directory;
            let (child, expected) = (Arc::clone(&here), top.id);
            here = Arc::new(blocking(&top.shown, move || back_to_parent(&child, expected)).await?);
            continue;
        };
        let path = top.shown.join(OsStr::from_bytes(&raw));
        let os_name = PathBuf::from(OsStr::from_bytes(&raw));
        let name = utf8(raw, &path)?;
        let dir = Arc::clone(&here);
        match kind {
            Kind::Other => {}
            Kind::File { .. } => {
                let max = budget.bytes_left();
                let read =
                    blocking(&path, move || read_file(&dir, os_name.as_os_str(), max)).await?;
                let (blob, executable) = budget.file(read, &path)?;
                let digest = blob.store(store, &path).await?;
                let top = stack.last_mut().unwrap_or(&mut root);
                top.directory.files.push(FileNode {
                    name,
                    digest: Some(digest),
                    is_executable: executable,
                    ..FileNode::default()
                });
            }
            Kind::Symlink => {
                let raw = blocking(&path, move || read_link(&dir, os_name.as_os_str())).await?;
                top.directory.symlinks.push(SymlinkNode {
                    name,
                    target: utf8(raw, &path)?,
                    ..SymlinkNode::default()
                });
            }
            Kind::Directory => {
                // The root is level 0 and `stack` holds the levels below it, so this
                // directory is level `stack.len() + 1`.
                let max_depth = budget.limits.max_depth;
                if stack.len() >= max_depth {
                    return Err(limit(&path, Exceeded::Depth, max_depth as u64));
                }
                let slot = children.len();
                let (fd, frame) = open_frame(budget, dir, os_name, path, name, slot).await?;
                // Held by the slot until the frame is done, so `children` stays in
                // pre-order.
                children.push(Directory::default());
                stack.push(frame);
                here = fd;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh scratch directory for one unit test, beside the test binary.
    fn scratch(name: &str) -> PathBuf {
        let exe = std::env::current_exe().expect("test binary");
        let dir = exe
            .parent()
            .expect("deps directory")
            .join("kbf-outputs-unit")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn open(dir: &Path) -> OwnedFd {
        rustix::fs::openat(CWD, dir, DIRECTORY, Mode::empty()).expect("open")
    }

    /// What `read_file` found, comparable: the bytes (a file's read from where it is
    /// handed over), the digest a file comes with (`None` for bytes), and the
    /// executable bit.
    fn contents(found: Option<(Blob, bool)>) -> Option<(Vec<u8>, Option<Digest>, bool)> {
        found.map(|(blob, executable)| match blob {
            Blob::Bytes(bytes) => (bytes, None, executable),
            Blob::File(mut file, digest) => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).expect("read back");
                (bytes, Some(digest), executable)
            }
        })
    }

    /// Catches a file read through a descriptor that is not a regular file: an entry
    /// replaced by a FIFO between the `stat` and the `open` must fail, not block or be
    /// stored as a file.
    #[test]
    fn a_file_that_is_no_longer_regular_is_not_read() {
        let dir = scratch("fifo");
        let fd = open(&dir);
        let made = std::process::Command::new("mkfifo")
            .arg(dir.join("fifo"))
            .status()
            .expect("mkfifo");
        assert!(made.success());
        let why = read_file(&fd, OsStr::new("fifo"), u64::MAX).expect_err("a FIFO");
        assert!(
            why.to_string().contains("no longer a regular file"),
            "{why}"
        );
    }

    /// Catches the byte bound being off by one or ignored, for a small file and for
    /// one large enough to be hashed in chunks; and a large file stored from anywhere
    /// but its start, or with a digest other than its bytes'.
    #[test]
    fn the_byte_bound_holds_for_small_and_chunked_files() {
        let dir = scratch("bytes");
        let fd = open(&dir);
        std::fs::write(dir.join("small"), b"bytes").expect("write");
        let read = |name: &str, max| read_file(&fd, OsStr::new(name), max).expect("read");
        // A small file within the bound is read whole, as bytes.
        assert_eq!(
            contents(read("small", 5)),
            Some((b"bytes".to_vec(), None, false))
        );
        assert!(read("small", 4).is_none());

        let large: Vec<u8> = (0..(2 * CHUNK_BYTES + 7))
            .map(|i| (i % 253) as u8)
            .collect();
        std::fs::write(dir.join("large"), &large).expect("write");
        let size = large.len() as u64;
        // A large file within the bound is handed over as a file, at its start, with
        // the digest of its bytes.
        assert_eq!(
            contents(read("large", size)),
            Some((large.clone(), Some(digest_of(&large)), false))
        );
        assert!(read("large", size - 1).is_none());
        // Past the bound within the first chunk of a large file.
        assert!(read("large", 10).is_none());
        // Exactly one chunk is a file, not bytes.
        std::fs::write(dir.join("chunk"), vec![1u8; CHUNK_BYTES]).expect("write");
        assert!(matches!(
            read("chunk", u64::MAX),
            Some((Blob::File(..), false))
        ));
    }

    /// Catches the walk going back up into a directory other than the one it came
    /// from (a parent renamed away and replaced while it was below).
    #[test]
    fn the_walk_goes_back_only_where_it_came_from() {
        let dir = scratch("parent");
        std::fs::create_dir(dir.join("child")).expect("mkdir");
        let top = open(&dir);
        let child = rustix::fs::openat(&top, "child", DIRECTORY, Mode::empty()).expect("open");
        let id = identity(&top).expect("identity");
        let back = back_to_parent(&child, id).expect("the same parent");
        assert_eq!(identity(&back).expect("identity"), id);
        let why = back_to_parent(&child, (id.0, id.1 ^ 1)).expect_err("another directory");
        assert!(
            why.to_string().contains("parent directory was replaced"),
            "{why}"
        );
    }

    /// Catches a blocking task that panicked being lost or taken for success.
    #[tokio::test]
    async fn a_blocking_task_that_panicked_is_an_error() {
        let shown = Path::new("shown/path");
        let outcome: Result<(), OutputsError> = blocking(shown, || panic!("walk panicked")).await;
        let why = outcome.expect_err("the task panicked").to_string();
        assert!(why.starts_with("shown/path: "), "{why}");
        assert!(why.contains("walk panicked"), "{why}");
    }

    /// Catches a working directory or output path that climbs out (`..`), is absolute,
    /// or holds an empty component, being walked instead of refused.
    #[test]
    fn paths_that_leave_the_root_are_refused() {
        for bad in ["..", "a/../b", "/abs", "a//b", ".", "a/./b", "a\0b"] {
            assert!(
                matches!(
                    check_relative("output path", bad),
                    Err(OutputsError::Invalid(_))
                ),
                "{bad:?}"
            );
        }
        for good in ["", "a", "a/b.c", "..a"] {
            assert!(check_relative("output path", good).is_ok(), "{good:?}");
        }
    }
}
