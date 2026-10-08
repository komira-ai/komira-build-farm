//! Files in and out of a lease: the input root written from the CAS into a directory,
//! and the action's outputs read back into the CAS.
//!
//! Both drivers write their input roots with [`materialize`] and check a Command's
//! paths with [`check_relative`] and [`output_paths`]. The native driver reads outputs
//! with `kbf-outputs` and the container driver with its own descriptor walk over the
//! overlay's upper directory; [`collect`] here serves `LocalRuntime` (tests only).
//!
//! Directory messages come from clients, so every name is checked before it touches the
//! host's filesystem: a name with a slash, `.`, `..` or a NUL, or a name used twice in
//! one directory, refuses the action. Files are created with `O_EXCL` in directories this
//! module created, so no write follows a symlink the input tree planted.
//!
//! Outputs are found from the input root, not from the working directory's host path:
//! every directory on the way to an output, the working directory and those above it
//! included, must still be a real directory (`symlink_metadata`), so an output under a
//! directory the action replaced with a symlink is left out rather than read from
//! wherever the link points. An output that is a symlink is reported as one and never
//! followed. A file is opened with `O_NOFOLLOW` and checked again on the open
//! descriptor, so one swapped for a symlink or a FIFO after the check is refused, not
//! read. That guards the last step only: a runtime must end every process of the
//! action before [`collect`], or one left running could swap a directory on the way
//! between the check and the read.

use std::collections::BTreeSet;
use std::future::Future;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use kbf_proto::reapi::{
    ActionResult, Command, Digest, Directory, DirectoryNode, FileNode, OutputDirectory, OutputFile,
    OutputSymlink, SymlinkNode, Tree,
};
use prost::Message;
use tokio::fs;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt};

use crate::cas::{Cas, CasError, digest_of, fetch, label};

/// Why the input root could not be written or the outputs read.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    /// The action's tree or paths break a rule: the client's error.
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Cas(#[from] CasError),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> TreeError + '_ {
    move |source| TreeError::Io {
        path: path.to_owned(),
        source,
    }
}

/// Fetches a blob and decodes it as `M`.
///
/// # Errors
/// The blob cannot be fetched, or does not decode as `M` ([`TreeError::Invalid`]).
pub async fn fetch_message<M: Message + Default>(
    cas: &impl Cas,
    digest: &Digest,
) -> Result<M, TreeError> {
    let bytes = fetch(cas, digest).await?;
    M::decode(bytes.as_slice())
        .map_err(|e| TreeError::Invalid(format!("blob {} does not decode: {e}", label(digest))))
}

/// Checks one name of a Directory entry.
fn check_name(name: &str) -> Result<(), TreeError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(TreeError::Invalid(format!(
            "input tree entry name {name:?} is not a single path component"
        )));
    }
    Ok(())
}

/// Checks a path the Command names (working directory, output path): relative, made
/// of non-empty components other than `.` and `..`, joined by single slashes. The empty
/// path is the directory itself.
///
/// # Errors
/// [`TreeError::Invalid`], naming `what`.
pub fn check_relative(what: &str, path: &str) -> Result<(), TreeError> {
    let plain = path.is_empty()
        || path
            .split('/')
            .all(|part| !matches!(part, "" | "." | "..") && !part.contains('\0'));
    if !plain {
        return Err(TreeError::Invalid(format!(
            "{what} {path:?} must be relative and must not contain . or .."
        )));
    }
    Ok(())
}

/// Writes the tree under `root` into the empty directory `dir`.
///
/// # Errors
/// A blob is missing or corrupt, the tree breaks a naming rule, or a write fails.
pub async fn materialize(cas: &impl Cas, root: &Digest, dir: &Path) -> Result<(), TreeError> {
    let mut pending = vec![(root.clone(), dir.to_owned())];
    while let Some((digest, here)) = pending.pop() {
        let directory: Directory = fetch_message(cas, &digest).await?;
        let mut seen = BTreeSet::new();
        let names = directory
            .files
            .iter()
            .map(|f| &f.name)
            .chain(directory.directories.iter().map(|d| &d.name))
            .chain(directory.symlinks.iter().map(|s| &s.name));
        for name in names {
            check_name(name)?;
            if !seen.insert(name) {
                return Err(TreeError::Invalid(format!(
                    "input tree names {name:?} twice in one directory"
                )));
            }
        }
        for file in &directory.files {
            write_file(cas, file, &here.join(&file.name)).await?;
        }
        for link in &directory.symlinks {
            let path = here.join(&link.name);
            fs::symlink(&link.target, &path).await.map_err(io(&path))?;
        }
        for sub in &directory.directories {
            let path = here.join(&sub.name);
            let digest = sub.digest.clone().ok_or_else(|| {
                TreeError::Invalid(format!("input directory {:?} has no digest", sub.name))
            })?;
            fs::create_dir(&path).await.map_err(io(&path))?;
            pending.push((digest, path));
        }
    }
    Ok(())
}

async fn write_file(cas: &impl Cas, node: &FileNode, path: &Path) -> Result<(), TreeError> {
    let digest = node
        .digest
        .as_ref()
        .ok_or_else(|| TreeError::Invalid(format!("input file {:?} has no digest", node.name)))?;
    let bytes = fetch(cas, digest).await?;
    let mode = if node.is_executable { 0o755 } else { 0o644 };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .await
        .map_err(io(path))?;
    file.write_all(&bytes).await.map_err(io(path))?;
    file.flush().await.map_err(io(path))?;
    Ok(())
}

/// The output paths a Command declares, each checked. REAPI 2.1 lists them in
/// `output_paths`; older clients list `output_files` and `output_directories`.
///
/// # Errors
/// A path is empty, absolute, or contains `.` or `..`.
#[allow(deprecated)]
pub fn output_paths(command: &Command) -> Result<Vec<String>, TreeError> {
    let paths: Vec<String> = if command.output_paths.is_empty() {
        command
            .output_files
            .iter()
            .chain(&command.output_directories)
            .cloned()
            .collect()
    } else {
        command.output_paths.clone()
    };
    for path in &paths {
        if path.is_empty() {
            return Err(TreeError::Invalid("an output path is empty".to_owned()));
        }
        check_relative("output path", path)?;
    }
    Ok(paths)
}

/// Creates `base/relative` and every directory on the way that does not exist yet.
/// A component that exists as anything but a directory (a symlink the input tree
/// planted, a file) refuses the action: following it would put the action's work, or
/// its outputs, outside the lease's directory. `relative` is a checked path.
///
/// # Errors
/// [`TreeError::Invalid`] for a component that is not a directory; I/O failures.
pub async fn real_dirs(base: &Path, relative: &str) -> Result<PathBuf, TreeError> {
    let mut at = base.to_owned();
    for part in relative.split('/').filter(|p| !p.is_empty()) {
        at.push(part);
        match fs::create_dir(&at).await {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(&at).await.map_err(io(&at))?;
                if !meta.is_dir() {
                    return Err(TreeError::Invalid(format!(
                        "{relative:?} passes through {part:?}, which the input tree makes \
                         something other than a directory"
                    )));
                }
            }
            Err(e) => return Err(io(&at)(e)),
        }
    }
    Ok(at)
}

/// `dir/path` if every directory on the way to it is a real directory, `None` if one is
/// absent or is not a directory (a symlink, a file).
async fn under(dir: &Path, path: &str) -> Result<Option<PathBuf>, TreeError> {
    let mut at = dir.to_owned();
    let mut parts: Vec<&str> = path.split('/').collect();
    let leaf = parts.pop().unwrap_or_default();
    for part in parts {
        at.push(part);
        match fs::symlink_metadata(&at).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(&at)(e)),
        }
    }
    at.push(leaf);
    Ok(Some(at))
}

/// Reads the regular file at `path`: its bytes and whether it is executable. The last
/// component is not followed (`O_NOFOLLOW`) and the type is checked on the open
/// descriptor, so a file swapped for a symlink, a FIFO or a device after its caller
/// checked it is refused ([`TreeError::Invalid`]) rather than read. `O_NONBLOCK` keeps
/// a FIFO from blocking the open; it changes nothing for a regular file.
async fn read_file(path: &Path) -> Result<(Vec<u8>, bool), TreeError> {
    let changed = || {
        TreeError::Invalid(format!(
            "{} stopped being a regular file while the outputs were read",
            path.display()
        ))
    };
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(changed()),
        Err(e) => return Err(io(path)(e)),
    };
    let meta = file.metadata().await.map_err(io(path))?;
    if !meta.is_file() {
        return Err(changed());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.map_err(io(path))?;
    Ok((bytes, meta.permissions().mode() & 0o111 != 0))
}

/// Reads each output path into the CAS and records it in `result`. `root` is the input
/// root's directory as the action left it; `working_directory` (a checked path) and
/// each output path are relative paths below it, and every directory on the way from
/// `root` must be a real one. A path the action did not create is left out, as is one
/// under a directory the action replaced with a file or a symlink (the working
/// directory included), and an entry that is not a file, directory or symlink (a
/// socket, a FIFO). Each path in `result` is relative to the working directory.
///
/// # Errors
/// A read or an upload fails; a file changes type while it is read
/// ([`TreeError::Invalid`]).
pub async fn collect(
    cas: &impl Cas,
    root: &Path,
    working_directory: &str,
    paths: &[String],
    result: &mut ActionResult,
) -> Result<(), TreeError> {
    for path in paths {
        let from_root = if working_directory.is_empty() {
            path.clone()
        } else {
            format!("{working_directory}/{path}")
        };
        let Some(host) = under(root, &from_root).await? else {
            continue;
        };
        let meta = match fs::symlink_metadata(&host).await {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(io(&host)(e)),
        };
        if meta.is_file() {
            let (bytes, is_executable) = read_file(&host).await?;
            result.output_files.push(OutputFile {
                path: path.clone(),
                digest: Some(cas.put(bytes).await?),
                is_executable,
                ..OutputFile::default()
            });
        } else if meta.is_dir() {
            let (root, children) = tree(cas, host).await?;
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
        } else if meta.is_symlink() {
            let target = fs::read_link(&host).await.map_err(io(&host))?;
            result.output_symlinks.push(OutputSymlink {
                path: path.clone(),
                target: target.to_string_lossy().into_owned(),
                ..OutputSymlink::default()
            });
        }
    }
    Ok(())
}

type TreeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(Directory, Vec<Directory>), TreeError>> + Send + 'a>>;

/// The Directory for `dir`, with every Directory below it, storing each file.
fn tree<C: Cas>(cas: &C, dir: PathBuf) -> TreeFuture<'_> {
    Box::pin(async move {
        let mut entries = Vec::new();
        let mut reader = fs::read_dir(&dir).await.map_err(io(&dir))?;
        while let Some(entry) = reader.next_entry().await.map_err(io(&dir))? {
            entries.push(entry);
        }
        // REAPI wants each list sorted by name.
        entries.sort_by_key(tokio::fs::DirEntry::file_name);
        let mut directory = Directory::default();
        let mut below = Vec::new();
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = fs::symlink_metadata(&path).await.map_err(io(&path))?;
            if meta.is_file() {
                let (bytes, is_executable) = read_file(&path).await?;
                directory.files.push(FileNode {
                    name,
                    digest: Some(cas.put(bytes).await?),
                    is_executable,
                    ..FileNode::default()
                });
            } else if meta.is_dir() {
                let (sub, mut subs) = tree(cas, path).await?;
                directory.directories.push(DirectoryNode {
                    name,
                    digest: Some(digest_of(&sub.encode_to_vec())),
                });
                below.push(sub);
                below.append(&mut subs);
            } else if meta.is_symlink() {
                let target = fs::read_link(&path).await.map_err(io(&path))?;
                directory.symlinks.push(SymlinkNode {
                    name,
                    target: target.to_string_lossy().into_owned(),
                    ..SymlinkNode::default()
                });
            }
        }
        Ok((directory, below))
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Catches a file read through a symlink, or from a FIFO or a device, that
    /// replaced it after its check (the daemon would upload a host file, or block on
    /// the FIFO); and an I/O failure reported as that client error. A race cannot be
    /// timed from a test, so these read what a swap would leave: `/proc/self/exe` is
    /// a symlink, `/dev/null` a character device.
    #[tokio::test]
    async fn a_file_is_read_only_while_it_is_a_regular_file() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let (bytes, executable) = read_file(&manifest).await.expect("a regular file");
        assert_eq!(bytes, std::fs::read(&manifest).expect("read"));
        assert!(!executable);
        for swapped in ["/proc/self/exe", "/dev/null"] {
            let error = read_file(Path::new(swapped)).await.expect_err(swapped);
            assert!(
                matches!(error, TreeError::Invalid(_)),
                "{swapped}: {error:?}"
            );
            let message = error.to_string();
            assert!(
                message.contains("stopped being a regular file"),
                "{message}"
            );
        }
        let outcome = read_file(Path::new("/nonexistent/kbf-output")).await;
        assert!(matches!(outcome, Err(TreeError::Io { .. })), "{outcome:?}");
    }
}
