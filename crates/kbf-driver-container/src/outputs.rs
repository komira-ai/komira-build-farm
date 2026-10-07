//! Reading an action's outputs from the overlay's upper directory without following a
//! symlink at any level.
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
//! Each system call runs on tokio's blocking pool. At most one descriptor per
//! directory level is open at a time.

use std::ffi::OsStr;
use std::future::Future;
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use kbf_proto::reapi::{
    ActionResult, Directory, DirectoryNode, FileNode, OutputDirectory, OutputFile, OutputSymlink,
    SymlinkNode, Tree,
};
use prost::Message;
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::cas::{Cas, digest_of};
use crate::tree::{TreeError, check_relative};

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

/// The bytes and executable bit of the regular file `name` in `dir`. The type is
/// checked again on the open descriptor: an entry that stopped being a regular file
/// after it was examined is an error, never read.
fn read_file(dir: &OwnedFd, name: &OsStr) -> std::io::Result<(Vec<u8>, bool)> {
    let fd = rustix::fs::openat(dir, name, FILE, Mode::empty())?;
    let Kind::File { executable } = kind(&rustix::fs::fstat(&fd)?) else {
        return Err(std::io::Error::other(
            "is no longer a regular file once opened",
        ));
    };
    let mut bytes = Vec::new();
    std::fs::File::from(fd).read_to_end(&mut bytes)?;
    Ok((bytes, executable))
}

/// The target of the symlink `name` in `dir`, as written; never followed.
fn read_link(dir: &OwnedFd, name: &OsStr) -> std::io::Result<String> {
    let target = rustix::fs::readlinkat(dir, name, Vec::new())?;
    Ok(OsStr::from_bytes(target.as_bytes())
        .to_string_lossy()
        .into_owned())
}

/// The entries of `dir` (without `.` and `..`), sorted by name as REAPI wants, each
/// with what it is.
fn list(dir: &OwnedFd) -> std::io::Result<Vec<(Vec<u8>, Kind)>> {
    let mut entries = Vec::new();
    for entry in Dir::read_from(dir)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name == b"." || name == b".." {
            continue;
        }
        let stat = rustix::fs::statat(dir, OsStr::from_bytes(&name), AtFlags::SYMLINK_NOFOLLOW)?;
        entries.push((name, kind(&stat)));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

/// Reads each output path under `working_directory` in `upper` into the CAS and records
/// it in `result`. See the module comment for what is followed (nothing) and what is
/// left out.
pub(crate) async fn collect(
    cas: &impl Cas,
    upper: &Path,
    working_directory: &str,
    paths: &[String],
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
                let (parent, name) = (Arc::clone(&parent), name.clone());
                let (bytes, executable) =
                    blocking(&shown, move || read_file(&parent, name.as_os_str())).await?;
                result.output_files.push(OutputFile {
                    path: path.clone(),
                    digest: Some(cas.put(bytes).await?),
                    is_executable: executable,
                    ..OutputFile::default()
                });
            }
            Some(Kind::Directory) => {
                let (parent, name) = (Arc::clone(&parent), name.clone());
                let dir = blocking(&shown, move || {
                    Ok(rustix::fs::openat(
                        &parent,
                        name.as_os_str(),
                        DIRECTORY,
                        Mode::empty(),
                    )?)
                })
                .await?;
                let (root, children) = tree(cas, Arc::new(dir), shown).await?;
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
                let (parent, name) = (Arc::clone(&parent), name.clone());
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

type TreeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(Directory, Vec<Directory>), TreeError>> + Send + 'a>>;

/// The Directory for the open directory `dir` (shown as `shown`), with every Directory
/// below it, storing each file. Symlinks inside are recorded, never followed.
fn tree<C: Cas>(cas: &C, dir: Arc<OwnedFd>, shown: PathBuf) -> TreeFuture<'_> {
    Box::pin(async move {
        let entries = {
            let dir = Arc::clone(&dir);
            blocking(&shown, move || list(&dir)).await?
        };
        let mut directory = Directory::default();
        let mut below = Vec::new();
        for (raw, kind) in entries {
            let name = String::from_utf8_lossy(&raw).into_owned();
            let path = shown.join(OsStr::from_bytes(&raw));
            let os_name = PathBuf::from(OsStr::from_bytes(&raw));
            let dir = Arc::clone(&dir);
            match kind {
                Kind::Other => {}
                Kind::File { .. } => {
                    let (bytes, executable) =
                        blocking(&path, move || read_file(&dir, os_name.as_os_str())).await?;
                    directory.files.push(FileNode {
                        name,
                        digest: Some(cas.put(bytes).await?),
                        is_executable: executable,
                        ..FileNode::default()
                    });
                }
                Kind::Directory => {
                    let sub_fd = blocking(&path, move || {
                        Ok(rustix::fs::openat(
                            &dir,
                            os_name.as_os_str(),
                            DIRECTORY,
                            Mode::empty(),
                        )?)
                    })
                    .await?;
                    let (sub, mut subs) = tree(cas, Arc::new(sub_fd), path).await?;
                    directory.directories.push(DirectoryNode {
                        name,
                        digest: Some(digest_of(&sub.encode_to_vec())),
                    });
                    below.push(sub);
                    below.append(&mut subs);
                }
                Kind::Symlink => {
                    let target =
                        blocking(&path, move || read_link(&dir, os_name.as_os_str())).await?;
                    directory.symlinks.push(SymlinkNode {
                        name,
                        target,
                        ..SymlinkNode::default()
                    });
                }
            }
        }
        Ok((directory, below))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a file read through a descriptor that is not a regular file: an entry
    /// replaced by a FIFO between the `stat` and the `open` must fail, not block or be
    /// stored as a file.
    #[test]
    fn a_file_that_is_no_longer_regular_is_not_read() {
        let exe = std::env::current_exe().expect("test binary");
        let dir = exe
            .parent()
            .expect("deps directory")
            .join("kbf-driver-container-unit")
            .join("outputs-fifo");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        let fd = rustix::fs::openat(CWD, &dir, DIRECTORY, Mode::empty()).expect("open");
        rustix::fs::mknodat(&fd, "fifo", FileType::Fifo, Mode::RUSR | Mode::WUSR, 0)
            .expect("mkfifo");
        let why = read_file(&fd, OsStr::new("fifo")).expect_err("a FIFO");
        assert!(
            why.to_string().contains("no longer a regular file"),
            "{why}"
        );
        // A regular file beside it is read (llvm-cov scores each test binary's copy of
        // this function on its own, so this binary covers both arms).
        std::fs::write(dir.join("file"), b"bytes").expect("write");
        let read = read_file(&fd, OsStr::new("file")).expect("a regular file");
        assert_eq!(read, (b"bytes".to_vec(), false));
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
