//! Reading an action's outputs from the overlay's upper directory without following a
//! symlink at any level, within limits.
//!
//! The upper directory is the action's to shape. It can replace the working directory,
//! or any directory on an output's path, with a symlink to a host path
//! (`rm -rf d; ln -s /host/dir d`, output `d/key.pem`). So no output is read by path.
//! The upper directory is opened once, and every component below it, the working
//! directory's included, is opened relative to its parent's descriptor with
//! `O_NOFOLLOW`. A component on the way that is absent, a file or a symlink means the
//! action did not create the output there, and the output is left out. The last
//! component is examined with `AT_SYMLINK_NOFOLLOW`: a symlink is recorded as one,
//! never dereferenced, and a file is opened with `O_NOFOLLOW` and its type checked
//! again on the descriptor (an entry that changed is an error). The same holds inside
//! an output directory. `.`, `..` and empty components are refused before the walk.
//!
//! The action also decides how deep, how wide and how large its outputs are. An output
//! directory is walked iteratively, its stack of open directories a `Vec` on the heap,
//! so no depth of tree can overflow a thread's stack and take the daemon down. The
//! walk stops with [`TreeError::Limit`], failing the action, once its outputs pass any
//! of the [`OutputLimits`]: directory depth, entries, file bytes. Only the directory
//! being read holds a descriptor: the walk returns to a parent through the child's
//! `..`, and refuses a `..` that is not the directory it came from (device and inode).
//!
//! Each system call runs on tokio's blocking pool.

use std::ffi::OsStr;
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kbf_proto::reapi::{
    ActionResult, Directory, DirectoryNode, FileNode, OutputDirectory, OutputFile, OutputSymlink,
    SymlinkNode, Tree,
};
use prost::Message;
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::cas::{Cas, digest_of};
use crate::tree::{Exceeded, TreeError, check_relative};

/// How much output one action may leave. An action past a limit fails with
/// [`TreeError::Limit`], and none of its outputs is recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::Args)]
pub struct OutputLimits {
    /// The deepest directory an output directory may hold, in levels below it (its own
    /// entries are level 1).
    #[arg(long = "output-max-depth", default_value_t = OutputLimits::DEFAULT.max_depth)]
    pub max_depth: usize,
    /// The most entries (files, directories, symlinks and anything else) one action's
    /// outputs may hold together, the declared outputs included.
    #[arg(long = "output-max-entries", default_value_t = OutputLimits::DEFAULT.max_entries)]
    pub max_entries: u64,
    /// The most bytes one action's output files may hold together.
    #[arg(long = "output-max-bytes", default_value_t = OutputLimits::DEFAULT.max_bytes)]
    pub max_bytes: u64,
}

impl OutputLimits {
    /// 512 levels, a million entries, 16 GiB.
    pub const DEFAULT: Self = Self {
        max_depth: 512,
        max_entries: 1_000_000,
        max_bytes: 16 << 30,
    };
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What one action's outputs have used of its [`OutputLimits`] so far.
struct Budget {
    limits: OutputLimits,
    entries: u64,
    bytes: u64,
}

impl Budget {
    fn new(limits: OutputLimits) -> Self {
        Self {
            limits,
            entries: 0,
            bytes: 0,
        }
    }

    fn entries_left(&self) -> u64 {
        self.limits.max_entries.saturating_sub(self.entries)
    }

    fn bytes_left(&self) -> u64 {
        self.limits.max_bytes.saturating_sub(self.bytes)
    }

    /// Counts `n` entries found at `shown`.
    fn entries(&mut self, n: u64, shown: &Path) -> Result<(), TreeError> {
        if n > self.entries_left() {
            return Err(limit(shown, Exceeded::Entries, self.limits.max_entries));
        }
        self.entries += n;
        Ok(())
    }

    /// Counts the bytes of the file read at `shown`; `None` is one that did not fit.
    fn file(
        &mut self,
        read: Option<(Vec<u8>, bool)>,
        shown: &Path,
    ) -> Result<(Vec<u8>, bool), TreeError> {
        let Some((bytes, executable)) = read else {
            return Err(limit(shown, Exceeded::Bytes, self.limits.max_bytes));
        };
        self.bytes += bytes.len() as u64;
        Ok((bytes, executable))
    }
}

fn limit(shown: &Path, what: Exceeded, limit: u64) -> TreeError {
    TreeError::Limit {
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
    /// A socket, FIFO or device (an overlay whiteout is a character device): left out.
    Other,
}

fn kind(stat: &Stat) -> Kind {
    match FileType::from_raw_mode(stat.st_mode) {
        FileType::RegularFile => Kind::File {
            executable: stat.st_mode & 0o111 != 0,
        },
        FileType::Directory => Kind::Directory,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    }
}

/// Runs `f` on the blocking pool; a system call error is reported against `shown`
/// (a path for the message only, never opened).
async fn blocking<T: Send + 'static>(
    shown: &Path,
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, TreeError> {
    let io = |source: std::io::Error| TreeError::Io {
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

/// The bytes and executable bit of the regular file `name` in `dir`, or `None` when it
/// holds more than `max` bytes (at most `max + 1` are read). The type is checked again
/// on the open descriptor: an entry that stopped being a regular file after it was
/// examined is an error, never read.
fn read_file(dir: &OwnedFd, name: &OsStr, max: u64) -> std::io::Result<Option<(Vec<u8>, bool)>> {
    let fd = rustix::fs::openat(dir, name, FILE, Mode::empty())?;
    let Kind::File { executable } = kind(&rustix::fs::fstat(&fd)?) else {
        return Err(std::io::Error::other(
            "is no longer a regular file once opened",
        ));
    };
    // The read itself is bounded, not trusted to `st_size`: a file can grow.
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= max).then_some((bytes, executable)))
}

/// The target of the symlink `name` in `dir`, as written; never followed.
fn read_link(dir: &OwnedFd, name: &OsStr) -> std::io::Result<String> {
    let target = rustix::fs::readlinkat(dir, name, Vec::new())?;
    Ok(OsStr::from_bytes(target.as_bytes())
        .to_string_lossy()
        .into_owned())
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

fn identity(fd: &OwnedFd) -> std::io::Result<Identity> {
    let stat = rustix::fs::fstat(fd)?;
    Ok((stat.st_dev, stat.st_ino))
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

/// Reads each output path under `working_directory` in `upper` into the CAS and records
/// it in `result`, all of them together within `limits`. See the module comment for
/// what is followed (nothing) and what is left out.
pub(crate) async fn collect(
    cas: &impl Cas,
    upper: &Path,
    working_directory: &str,
    paths: &[String],
    limits: OutputLimits,
    result: &mut ActionResult,
) -> Result<(), TreeError> {
    check_relative("working directory", working_directory)?;
    let base = upper.join(working_directory);
    let upper_fd = {
        let upper = upper.to_owned();
        Arc::new(
            blocking(&base, move || {
                Ok(rustix::fs::openat(CWD, &upper, DIRECTORY, Mode::empty())?)
            })
            .await?,
        )
    };
    let wd_parts: Vec<String> = working_directory
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    let mut budget = Budget::new(limits);
    for path in paths {
        if path.is_empty() {
            return Err(TreeError::Invalid("an output path is empty".to_owned()));
        }
        check_relative("output path", path)?;
        let shown = base.join(path);
        let (dirs, name) = path.rsplit_once('/').unwrap_or(("", path));
        let mut parts: Vec<String> = wd_parts.clone();
        parts.extend(
            dirs.split('/')
                .filter(|part| !part.is_empty())
                .map(str::to_owned),
        );
        let name = PathBuf::from(name);
        let start = Arc::clone(&upper_fd);
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
                let (bytes, executable) = budget.file(read, &shown)?;
                result.output_files.push(OutputFile {
                    path: path.clone(),
                    digest: Some(cas.put(bytes).await?),
                    is_executable: executable,
                    ..OutputFile::default()
                });
            }
            Some(Kind::Directory) => {
                budget.entries(1, &shown)?;
                let (root, children) = walk(cas, &mut budget, parent, name, shown).await?;
                let root_digest = digest_of(&root.encode_to_vec());
                let tree = Tree {
                    root: Some(root),
                    children,
                };
                result.output_directories.push(OutputDirectory {
                    path: path.clone(),
                    tree_digest: Some(cas.put(tree.encode_to_vec()).await?),
                    is_topologically_sorted: false,
                    root_directory_digest: Some(root_digest),
                });
            }
            Some(Kind::Symlink) => {
                budget.entries(1, &shown)?;
                let target = blocking(&shown, move || read_link(&parent, name.as_os_str())).await?;
                result.output_symlinks.push(OutputSymlink {
                    path: path.clone(),
                    target,
                    ..OutputSymlink::default()
                });
            }
        }
    }
    Ok(())
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
) -> Result<(Arc<OwnedFd>, Frame), TreeError> {
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
/// never followed.
///
/// MUTANT: recursive again, once per level, and no depth limit.
fn walk<'a, C: Cas>(
    cas: &'a C,
    budget: &'a mut Budget,
    parent: Arc<OwnedFd>,
    name: PathBuf,
    shown: PathBuf,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<(Directory, Vec<Directory>), TreeError>>
            + Send
            + 'a,
    >,
> {
    Box::pin(async move {
        let (here, mut frame) = open_frame(budget, parent, name, shown, String::new(), 0).await?;
        let mut below = Vec::new();
        while let Some((raw, kind)) = frame.entries.next() {
            let name = String::from_utf8_lossy(&raw).into_owned();
            let path = frame.shown.join(OsStr::from_bytes(&raw));
            let os_name = PathBuf::from(OsStr::from_bytes(&raw));
            let dir = Arc::clone(&here);
            match kind {
                Kind::Other => {}
                Kind::File { .. } => {
                    let max = budget.bytes_left();
                    let read =
                        blocking(&path, move || read_file(&dir, os_name.as_os_str(), max)).await?;
                    let (bytes, executable) = budget.file(read, &path)?;
                    frame.directory.files.push(FileNode {
                        name,
                        digest: Some(cas.put(bytes).await?),
                        is_executable: executable,
                        ..FileNode::default()
                    });
                }
                Kind::Symlink => {
                    let target =
                        blocking(&path, move || read_link(&dir, os_name.as_os_str())).await?;
                    frame.directory.symlinks.push(SymlinkNode {
                        name,
                        target,
                        ..SymlinkNode::default()
                    });
                }
                Kind::Directory => {
                    let (sub, mut subs) = walk(cas, &mut *budget, dir, os_name, path).await?;
                    frame.directory.directories.push(DirectoryNode {
                        name,
                        digest: Some(digest_of(&sub.encode_to_vec())),
                    });
                    below.push(sub);
                    below.append(&mut subs);
                }
            }
        }
        let _ = (
            frame.id,
            frame.name,
            frame.slot,
            back_to_parent,
            Exceeded::Depth,
        );
        Ok((frame.directory, below))
    })
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
            .join("kbf-driver-container-unit")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// Catches a file read through a descriptor that is not a regular file: an entry
    /// replaced by a FIFO between the `stat` and the `open` must fail, not block or be
    /// stored as a file. Also catches the byte bound being off by one or ignored: a
    /// file of exactly `max` bytes is read, one byte more is refused.
    #[test]
    fn a_file_that_is_no_longer_regular_is_not_read() {
        let dir = scratch("outputs-fifo");
        let fd = rustix::fs::openat(CWD, &dir, DIRECTORY, Mode::empty()).expect("open");
        rustix::fs::mknodat(&fd, "fifo", FileType::Fifo, Mode::RUSR | Mode::WUSR, 0)
            .expect("mkfifo");
        let why = read_file(&fd, OsStr::new("fifo"), u64::MAX).expect_err("a FIFO");
        assert!(
            why.to_string().contains("no longer a regular file"),
            "{why}"
        );
        // A regular file beside it is read (llvm-cov scores each test binary's copy of
        // this function on its own, so this binary covers both arms).
        std::fs::write(dir.join("file"), b"bytes").expect("write");
        let read = read_file(&fd, OsStr::new("file"), 5).expect("a regular file");
        assert_eq!(read, Some((b"bytes".to_vec(), false)));
        let read = read_file(&fd, OsStr::new("file"), 4).expect("a regular file");
        assert_eq!(read, None);
    }

    /// Catches the walk going back up into a directory other than the one it came
    /// from (a parent renamed away and replaced while it was below): `..` must be the
    /// directory it left, by device and inode.
    #[test]
    fn the_walk_goes_back_only_where_it_came_from() {
        let dir = scratch("outputs-parent");
        std::fs::create_dir(dir.join("child")).expect("mkdir");
        let top = rustix::fs::openat(CWD, &dir, DIRECTORY, Mode::empty()).expect("open");
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

    /// Catches a limit flag that is missing, misnamed or defaulted to something other
    /// than [`OutputLimits::DEFAULT`] in a command line that flattens the limits.
    #[test]
    fn the_limits_are_flags() {
        #[derive(clap::Parser)]
        struct Line {
            #[command(flatten)]
            limits: OutputLimits,
        }
        let parse = |args: &[&str]| {
            <Line as clap::Parser>::try_parse_from(
                std::iter::once("kbf").chain(args.iter().copied()),
            )
            .map(|line| line.limits)
        };
        assert_eq!(parse(&[]).expect("defaults"), OutputLimits::default());
        let set = [
            "--output-max-depth=7",
            "--output-max-entries=8",
            "--output-max-bytes=9",
        ];
        let limits = OutputLimits {
            max_depth: 7,
            max_entries: 8,
            max_bytes: 9,
        };
        assert_eq!(parse(&set).expect("set"), limits);
        assert!(parse(&["--output-max-depth=-1"]).is_err());
    }

    /// Catches a blocking task that panicked being lost or taken for success: it is an
    /// I/O error against the path being read.
    #[tokio::test]
    async fn a_blocking_task_that_panicked_is_an_error() {
        let shown = Path::new("shown/path");
        let outcome: Result<(), TreeError> = blocking(shown, || panic!("walk panicked")).await;
        let why = outcome.expect_err("the task panicked").to_string();
        assert!(why.starts_with("shown/path: "), "{why}");
        assert!(why.contains("walk panicked"), "{why}");
        // The same copy of `blocking`, finishing and failing as usual.
        blocking(shown, || Ok(())).await.expect("finished");
        let outcome: Result<(), TreeError> =
            blocking(shown, || Err(std::io::Error::other("refused"))).await;
        let why = outcome.expect_err("the call failed").to_string();
        assert_eq!(why, "shown/path: refused");
    }
}
