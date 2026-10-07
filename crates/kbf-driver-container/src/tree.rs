//! Files in and out of a lease: the input root written from the CAS into a directory,
//! and the action's outputs read back into the CAS.
//!
//! Directory messages come from clients, so every name is checked before it touches the
//! host's filesystem: a name with a slash, `.`, `..` or a NUL, or a name used twice in
//! one directory, refuses the action. Files are created with `O_EXCL` in directories this
//! module created, so no write follows a symlink the input tree planted. Outputs are read
//! by descriptor, component by component from the upper directory, and no symlink is
//! followed at any level.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use kbf_proto::reapi::{ActionResult, Digest, Directory, FileNode};
use prost::Message;
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::cas::{Cas, CasError, fetch, label};

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
pub(crate) async fn fetch_message<M: Message + Default>(
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
pub(crate) fn check_relative(what: &str, path: &str) -> Result<(), TreeError> {
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
            fs::create_dir(&path).await.map_err(io(&path))?;
            let digest = sub.digest.clone().ok_or_else(|| {
                TreeError::Invalid(format!("input directory {:?} has no digest", sub.name))
            })?;
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
#[allow(deprecated)]
pub fn output_paths(command: &kbf_proto::reapi::Command) -> Result<Vec<String>, TreeError> {
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

/// Refuses an output path that names an entry of the input root `root` (seen from
/// `working_directory`).
///
/// The driver reads outputs from the overlay's upper directory, which holds only what
/// the action created or changed. An output that is already an input would come back
/// partial: an unchanged input file missing, an output directory without its unchanged
/// inputs, a deleted file as a skipped whiteout. REAPI allows such actions; Bazel and
/// Buck2 never send them. So they are refused as the client's error rather than run and
/// cached incomplete.
///
/// The walk follows no symlink. A component that is absent, or that is not a
/// directory, ends it with "no overlap": the driver makes each output's parent a
/// directory in the upper layer, and an upper directory hides a lower file or symlink
/// of the same name instead of merging with it.
pub async fn refuse_outputs_in_inputs(
    root: &Path,
    working_directory: &str,
    outputs: &[String],
) -> Result<(), TreeError> {
    for output in outputs {
        // Collected before the walk: a closure-holding iterator kept across an await
        // makes the future not `Send` for every lifetime.
        let parts: Vec<&str> = working_directory
            .split('/')
            .filter(|part| !part.is_empty())
            .chain(output.split('/'))
            .collect();
        let mut host = root.to_owned();
        for (i, part) in parts.iter().enumerate() {
            host.push(part);
            let meta = match fs::symlink_metadata(&host).await {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(io(&host)(e)),
            };
            if i + 1 == parts.len() {
                return Err(TreeError::Invalid(format!(
                    "output path {output:?} names an entry of the input root; outputs that \
                     overlap the inputs are not supported"
                )));
            }
            if !meta.is_dir() {
                break;
            }
        }
    }
    Ok(())
}

/// Refuses a working directory that the input root `root` has as anything but
/// directories: a symlink or a file on its path.
///
/// The driver makes the working directory in the overlay's upper directory, and an
/// upper directory hides a lower symlink or file of the same name, so the action would
/// start in an empty directory rather than among its inputs. The walk follows no
/// symlink; a component that is absent ends it (the driver makes the rest).
pub async fn refuse_hidden_working_directory(
    root: &Path,
    working_directory: &str,
) -> Result<(), TreeError> {
    let mut host = root.to_owned();
    for part in working_directory.split('/').filter(|part| !part.is_empty()) {
        host.push(part);
        match fs::symlink_metadata(&host).await {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                return Err(TreeError::Invalid(format!(
                    "working directory {working_directory:?} is not a directory of the \
                     input root (a symlink or file is on its path)"
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(io(&host)(e)),
        }
    }
    Ok(())
}

/// Reads each output path, under `working_directory` in the overlay's upper directory
/// `upper`, into the CAS and records it in `result`.
///
/// Nothing is followed: every component from `upper` down, the working directory's
/// included, is opened without following a symlink, and a declared output that is a
/// symlink is recorded as an `OutputSymlink` (see `outputs`). A path the action did
/// not create is left out, including one whose parent (or working directory) the
/// action replaced with a file or a symlink; an entry that is not a file, directory or
/// symlink (a socket, a FIFO, a whiteout) is left out too. A working directory or
/// output path with an empty, `.` or `..` component is refused. No output overlaps the
/// input root (`refuse_outputs_in_inputs`), so every output is whole in the upper
/// directory.
pub async fn collect(
    cas: &impl Cas,
    upper: &Path,
    working_directory: &str,
    paths: &[String],
    result: &mut ActionResult,
) -> Result<(), TreeError> {
    crate::outputs::collect(cas, upper, working_directory, paths, result).await
}
