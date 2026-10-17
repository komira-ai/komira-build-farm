//! Applying image layers to a root filesystem, in order.
//!
//! Run in a user namespace whose root is the action's root (a helper container of the
//! container driver), so the files it makes are owned by the ids the action sees as the
//! image's own. What this guarantees, whatever the tars hold:
//! - every path is resolved inside the root: `..` in an entry's name is refused, a
//!   symlink met on the way is followed as a chroot would follow it (an absolute target
//!   starts at the root, `..` stops at the root), and a symlink is never written
//!   through to a place outside;
//! - a `.wh.<name>` entry deletes `<name>` as the lower layers left it, and
//!   `.wh..wh..opq` deletes what the lower layers left in its directory; neither
//!   touches what the same layer wrote, and neither is written itself;
//! - each entry gets its owner (an id above the 65,536 the namespace maps is an error),
//!   then its mode: `chown` clears the setuid and setgid bits, so the mode goes last.
//!   A directory's mode and times are set when its layer is done, so a read-only
//!   directory does not stop its own contents;
//! - device nodes and fifos are skipped, hard links are made inside the root, and the
//!   bytes of files are bounded.

use std::collections::{HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, CWD, Gid, Timestamps, Uid};
use tar::EntryType;

use crate::UnpackError;
use crate::manifest::Compression;

/// The ids a container's user namespace maps (0 to 65535).
pub const MAX_ID: u32 = 65_535;

/// Symlinks followed in resolving one path before it is called a loop.
const MAX_HOPS: u32 = 40;

/// The word a whiteout name starts with, and the names that are not whiteouts of a file.
const WHITEOUT: &str = ".wh.";
const HOUSEKEEPING: &str = ".wh..wh.";
const OPAQUE: &str = ".wh..wh..opq";

/// Whose files the unpacked ones are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// The owner each tar header names.
    Header,
    /// Every file is given to one owner; the header's ids are still range-checked. For a
    /// process that may not give files to other ids.
    AllTo { uid: u32, gid: u32 },
}

/// What an unpack did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unpacked {
    /// Entries applied (files, directories, links, whiteouts).
    pub entries: u64,
    /// Entries skipped: device nodes, fifos, global headers, housekeeping whiteouts.
    pub skipped: u64,
    /// Bytes of regular files written.
    pub bytes: u64,
}

/// An unpack of layers into a root.
#[derive(Clone, Copy, Debug)]
pub struct Unpack {
    ownership: Ownership,
    max_bytes: u64,
}

impl Unpack {
    /// An unpack with the default bound on file bytes (32 GiB).
    #[must_use]
    pub fn new(ownership: Ownership) -> Self {
        Self {
            ownership,
            max_bytes: 32 << 30,
        }
    }

    /// Bounds the bytes of regular files all layers may hold.
    #[must_use]
    pub fn max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Makes the directory `rootfs` and applies `layers` (lowest first) to it.
    ///
    /// # Errors
    /// `rootfs` exists, a tar is unreadable or holds a path that leaves the root, a
    /// symlink loop, an owner out of range or too many bytes, or a file operation fails.
    pub fn run(
        &self,
        rootfs: &Path,
        layers: &[(PathBuf, Compression)],
    ) -> Result<Unpacked, UnpackError> {
        std::fs::create_dir(rootfs).map_err(io(rootfs))?;
        std::fs::set_permissions(rootfs, std::fs::Permissions::from_mode(0o755))
            .map_err(io(rootfs))?;
        let mut state = State {
            unpack: *self,
            root: Root(rootfs.to_owned()),
            counts: Unpacked::default(),
        };
        for (path, compression) in layers {
            state.layer(path, *compression)?;
        }
        Ok(state.counts)
    }
}

/// `source` as an error about `path`.
fn io(path: &Path) -> impl FnOnce(std::io::Error) -> UnpackError {
    let path = path.to_owned();
    move |source| UnpackError::Io { path, source }
}

/// A root directory, and paths resolved inside it.
struct Root(PathBuf);

impl Root {
    /// The host path of the root-relative components `parts`.
    fn path(&self, parts: &[OsString]) -> PathBuf {
        let mut path = self.0.clone();
        path.extend(parts);
        path
    }

    /// The components `rel` names once every symlink on the way is followed as a chroot
    /// would: the last one too only if `follow_last`. Components that do not exist yet
    /// are kept as they are.
    fn resolve(&self, rel: &[OsString], follow_last: bool) -> Result<Vec<OsString>, UnpackError> {
        let mut done: Vec<OsString> = Vec::new();
        let mut todo: VecDeque<OsString> = rel.iter().cloned().collect();
        let mut hops = 0;
        while let Some(name) = todo.pop_front() {
            if name == ".." {
                done.pop();
                continue;
            }
            done.push(name);
            let path = self.path(&done);
            let is_link = std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_symlink());
            if !is_link || (todo.is_empty() && !follow_last) {
                continue;
            }
            hops += 1;
            if hops > MAX_HOPS {
                return Err(UnpackError::SymlinkLoop(path));
            }
            let target = std::fs::read_link(&path).map_err(io(&path))?;
            done.pop();
            if target.is_absolute() {
                done.clear();
            }
            let parts: Vec<OsString> = target
                .components()
                .filter_map(|c| match c {
                    Component::Normal(n) => Some(n.to_owned()),
                    Component::ParentDir => Some(OsString::from("..")),
                    _ => None,
                })
                .collect();
            for part in parts.into_iter().rev() {
                todo.push_front(part);
            }
        }
        Ok(done)
    }
}

/// The components of an entry's name, with a leading `/` and `.` dropped.
///
/// # Errors
/// The name has a `..`.
fn clean(name: &[u8]) -> Result<Vec<OsString>, UnpackError> {
    let path = Path::new(OsStr::from_bytes(name));
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(n) => parts.push(n.to_owned()),
            Component::ParentDir => return Err(UnpackError::Unsafe(path.to_owned())),
            _ => {}
        }
    }
    Ok(parts)
}

/// Removes what is at `path`, whatever it is; nothing there is fine.
fn remove_any(path: &Path) -> Result<(), UnpackError> {
    let removed = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match removed {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(path)(e)),
        _ => Ok(()),
    }
}

/// A directory whose mode and time wait for the end of its layer.
struct Pending {
    path: PathBuf,
    mode: u32,
    mtime: u64,
}

struct State {
    unpack: Unpack,
    root: Root,
    counts: Unpacked,
}

/// One layer's progress.
struct Layer {
    /// The layer's file, for the errors that name it.
    file: PathBuf,
    /// Every path the layer wrote, and each ancestor of one, relative to the root.
    written: HashSet<Vec<OsString>>,
    dirs: Vec<Pending>,
}

impl Layer {
    /// A tar the layer holds that cannot be applied: `why`.
    fn broken(&self, why: String) -> UnpackError {
        UnpackError::Archive {
            layer: self.file.clone(),
            why,
        }
    }

    /// `read`, a number or a copy out of the tar, or the layer's error if the tar did not
    /// hold it.
    fn field<T>(&self, read: std::io::Result<T>) -> Result<T, UnpackError> {
        match read {
            Ok(value) => Ok(value),
            Err(e) => Err(self.broken(e.to_string())),
        }
    }
}

impl State {
    fn layer(&mut self, file: &Path, compression: Compression) -> Result<(), UnpackError> {
        let opened = std::fs::File::open(file).map_err(io(file))?;
        let reader: Box<dyn Read> = match compression {
            Compression::None => Box::new(std::io::BufReader::new(opened)),
            Compression::Gzip => Box::new(flate2::read::MultiGzDecoder::new(
                std::io::BufReader::new(opened),
            )),
        };
        let mut archive = tar::Archive::new(reader);
        let mut layer = Layer {
            file: file.to_owned(),
            written: HashSet::new(),
            dirs: Vec::new(),
        };
        // A fresh archive can always list its entries; reading one can fail.
        let entries = archive
            .entries()
            .expect("a fresh archive lists its entries");
        for entry in entries {
            let mut entry = layer.field(entry)?;
            self.entry(&mut layer, &mut entry)?;
        }
        for dir in layer.dirs.iter().rev() {
            set_mode(&dir.path, dir.mode)?;
            set_time(&dir.path, dir.mtime);
        }
        Ok(())
    }

    fn entry<R: Read>(
        &mut self,
        layer: &mut Layer,
        entry: &mut tar::Entry<'_, R>,
    ) -> Result<(), UnpackError> {
        let kind = entry.header().entry_type();
        let rel = clean(&entry.path_bytes())?;
        let Some((name, parent_rel)) = rel.split_last() else {
            // The root itself, as `./` in most layers.
            if kind == EntryType::Directory {
                let root = self.root.0.clone();
                self.directory(layer, entry.header(), &root)?;
            }
            return Ok(());
        };
        if let Some(target) = name.to_str().and_then(|n| n.strip_prefix(WHITEOUT)) {
            return self.whiteout(layer, parent_rel, name, target);
        }
        let parent = self.root.resolve(parent_rel, true)?;
        let parent_path = self.root.path(&parent);
        std::fs::create_dir_all(&parent_path).map_err(io(&parent_path))?;
        let dest = parent_path.join(name);
        let mut resolved = parent;
        resolved.push(name.clone());
        let header = entry.header().clone();
        match kind {
            EntryType::Directory => self.directory(layer, &header, &dest)?,
            EntryType::Regular | EntryType::Continuous => {
                self.counts.bytes = self.counts.bytes.saturating_add(entry.size());
                if self.counts.bytes > self.unpack.max_bytes {
                    return Err(UnpackError::TooBig(self.unpack.max_bytes));
                }
                remove_any(&dest)?;
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&dest)
                    .map_err(io(&dest))?;
                layer.field(std::io::copy(entry, &mut file))?;
                drop(file);
                self.metadata(layer, &header, &dest, true)?;
            }
            EntryType::Symlink => {
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| layer.broken("a symlink with no target".to_owned()))?;
                remove_any(&dest)?;
                std::os::unix::fs::symlink(OsStr::from_bytes(&target), &dest).map_err(io(&dest))?;
                self.metadata(layer, &header, &dest, false)?;
            }
            EntryType::Link => {
                let target = entry
                    .link_name_bytes()
                    .ok_or_else(|| layer.broken("a hard link with no target".to_owned()))?;
                let source = self.root.resolve(&clean(&target)?, false)?;
                remove_any(&dest)?;
                let source = self.root.path(&source);
                std::fs::hard_link(&source, &dest).map_err(io(&source))?;
            }
            EntryType::Char | EntryType::Block | EntryType::Fifo | EntryType::XGlobalHeader => {
                self.counts.skipped += 1;
                return Ok(());
            }
            other => {
                return Err(layer.broken(format!("an entry of type {other:?} is not supported")));
            }
        }
        for end in 1..=resolved.len() {
            layer.written.insert(resolved[..end].to_vec());
        }
        self.counts.entries += 1;
        Ok(())
    }

    /// A directory entry at `dest`: made if it is not a directory, its owner now, its
    /// mode and time at the end of the layer.
    fn directory(
        &mut self,
        layer: &mut Layer,
        header: &tar::Header,
        dest: &Path,
    ) -> Result<(), UnpackError> {
        if !std::fs::symlink_metadata(dest).is_ok_and(|m| m.is_dir()) {
            remove_any(dest)?;
            std::fs::create_dir(dest).map_err(io(dest))?;
        }
        self.own(layer, header, dest)?;
        layer.dirs.push(Pending {
            path: dest.to_owned(),
            mode: layer.field(header.mode())?,
            mtime: layer.field(header.mtime())?,
        });
        Ok(())
    }

    /// The owner, then the mode and time, of a file or symlink at `dest`.
    fn metadata(
        &self,
        layer: &Layer,
        header: &tar::Header,
        dest: &Path,
        has_mode: bool,
    ) -> Result<(), UnpackError> {
        self.own(layer, header, dest)?;
        if has_mode {
            set_mode(dest, layer.field(header.mode())?)?;
        }
        set_time(dest, layer.field(header.mtime())?);
        Ok(())
    }

    /// Gives `dest` (itself, never what a symlink points at) its owner.
    fn own(&self, layer: &Layer, header: &tar::Header, dest: &Path) -> Result<(), UnpackError> {
        let id = |value: std::io::Result<u64>| -> Result<u32, UnpackError> {
            let value = layer.field(value)?;
            u32::try_from(value)
                .ok()
                .filter(|v| *v <= MAX_ID)
                .ok_or_else(|| UnpackError::IdRange {
                    path: dest.to_owned(),
                    id: value,
                    max: MAX_ID,
                })
        };
        let (uid, gid) = (id(header.uid())?, id(header.gid())?);
        let (uid, gid) = match self.unpack.ownership {
            Ownership::Header => (uid, gid),
            Ownership::AllTo { uid, gid } => (uid, gid),
        };
        rustix::fs::chownat(
            CWD,
            dest,
            Some(Uid::from_raw(uid)),
            Some(Gid::from_raw(gid)),
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|e| io(dest)(e.into()))?;
        Ok(())
    }

    /// `.wh.<target>` (named `name`) in the directory `parent_rel`: deletes what lower
    /// layers left, not what this layer wrote.
    fn whiteout(
        &mut self,
        layer: &Layer,
        parent_rel: &[OsString],
        name: &OsString,
        target: &str,
    ) -> Result<(), UnpackError> {
        if name == OPAQUE {
            let dir = self.root.resolve(parent_rel, true)?;
            let path = self.root.path(&dir);
            let entries = match std::fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(io(&path)(e)),
            };
            for child in entries {
                let child = child.map_err(io(&path))?;
                let mut rel = dir.clone();
                rel.push(child.file_name());
                if !layer.written.contains(&rel) {
                    remove_any(&child.path())?;
                }
            }
        } else if name.to_str().is_some_and(|n| n.starts_with(HOUSEKEEPING)) {
            self.counts.skipped += 1;
            return Ok(());
        } else {
            if matches!(target, "" | "." | "..") {
                return Err(UnpackError::Unsafe(PathBuf::from(name)));
            }
            let mut rel = self.root.resolve(parent_rel, true)?;
            rel.push(OsString::from(target));
            if !layer.written.contains(&rel) {
                remove_any(&self.root.path(&rel))?;
            }
        }
        self.counts.entries += 1;
        Ok(())
    }
}

/// Sets the permission bits of `path` (not a symlink).
fn set_mode(path: &Path, mode: u32) -> Result<(), UnpackError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777)).map_err(io(path))
}

/// Sets the modification (and access) time of `path` itself, not what it points at: a
/// best effort, since a time that cannot be set is no reason to refuse an image.
fn set_time(path: &Path, mtime: u64) {
    let at = rustix::fs::Timespec {
        tv_sec: i64::try_from(mtime).unwrap_or(0),
        tv_nsec: 0,
    };
    let times = Timestamps {
        last_access: at,
        last_modification: at,
    };
    let _ = rustix::fs::utimensat(CWD, path, &times, AtFlags::SYMLINK_NOFOLLOW);
}

/// Makes sure the directories `dirs` and the empty files `files` (names relative to the
/// root, symlinks on the way followed inside it) exist in `rootfs`: the mount points a
/// container's runtime binds over. What is there is left as it is.
///
/// # Errors
/// A name leaves the root, a symlink loops, or something cannot be made.
pub fn ensure_paths(rootfs: &Path, dirs: &[&str], files: &[&str]) -> Result<(), UnpackError> {
    use std::os::unix::fs::DirBuilderExt as _;
    let root = Root(rootfs.to_owned());
    let place = |name: &str| -> Result<PathBuf, UnpackError> {
        let rel = clean(name.as_bytes())?;
        Ok(root.path(&root.resolve(&rel, true)?))
    };
    for dir in dirs {
        let path = place(dir)?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&path)
            .map_err(io(&path))?;
    }
    for file in files {
        let path = place(file)?;
        // A path under the root has a parent; `/` stands in for one that has none.
        let parent = path.parent().unwrap_or(Path::new("/"));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(parent)
            .map_err(io(parent))?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o644)
            .open(&path)
            .map_err(io(&path))?;
    }
    Ok(())
}
