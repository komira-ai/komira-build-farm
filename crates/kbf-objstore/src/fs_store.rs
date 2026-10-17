//! An [`ObjectStore`] in a directory on the local disk: the store of a single-host farm
//! that keeps its blobs across a restart without an S3 service (`kbf-server
//! --cold-store=file:///dir`).
//!
//! Layout under the directory:
//!
//! - `LOCK`: locked exclusively while a store is open on the directory, so a second
//!   open (another process, or this one) is refused rather than sharing it.
//! - `objects/<hh>/<h>`: one file per object. `<h>` is the SHA-256 of the key in 64
//!   lowercase hex digits and `<hh>` its first two, so every file name has the same
//!   length whatever the key (a key may be 1024 bytes, a file name 255) and the files
//!   spread over 256 directories, all created at open. A file is a header (`kbf-obj1`,
//!   the key's length as a big-endian `u16`, the key) and then the object's bytes.
//! - `tmp/`: objects being written. Emptied at open: a file there is a write that a
//!   crash or a failure cut short, and that was never answered.
//!
//! A write goes to a new file in `tmp/`, which is synced, then linked under its final
//! name, then unlinked from `tmp/`; then the final name's directory is synced, and only
//! then is the write answered. A link never replaces an existing name, so the store
//! has conditional writes: of two writers of one key, exactly one succeeds and the
//! other is answered [`ObjectStoreError::AlreadyExists`]. A crash never leaves a partial
//! object under its final name, and an answered write survives a power cut.
//!
//! A read checks that the file's header names the key asked for, and cuts the range at
//! the object's end as S3 does. A file too short to hold its header (a final name
//! whose bytes never reached the disk, which this write order does not produce but a
//! file system may after a crash) is read as no object at all: `NotFound`, so kbf
//! marks it missing and the blob is uploaded again; a listing skips it. The store does
//! not hash what it returns: kbf checks every blob it reads against its digest, from
//! any store. A listing's first page opens every object file to read its key (`list`
//! serves rebuild and the sweep only): file names are hashes, so key order is known only
//! once every header is read. The first [`LISTING_WINDOW`] keys under the prefix (after
//! the page's start) are kept, sorted, and the pages that follow are cut from them
//! without opening a file again, as long as their token came with a page of that
//! listing (it names the listing) within [`LISTING_KEPT_FOR`]; the contract lets a
//! listing miss writes made since it began. A listing of more keys than the window
//! reads every header again once its pages have used the window, keeping the next
//! window's keys: memory stays bounded by the window whatever the prefix holds, and a
//! listing of N keys reads the headers N / window times, rounded up. A listing started
//! [`ListToken::after`] a key is a new listing: it reads every header again, so it sees
//! every object written before it began. Nothing reads the objects at open: on a large directory that
//! would hold up every start. [`FsStore::scan`] reads one shard's headers for the
//! server's start-up pass, which runs in the background after the server serves (it
//! deletes orphans and forgets index entries whose file is gone; `kbf-front`'s
//! `Cache::consistency_pass`). Until it reaches an object, a lost or damaged one is
//! found when it is read, or by [`ObjectStore::may_hold`] before an action-cache hit
//! is served.
//!
//! A write is refused when the file system has less free space than the store's floor
//! ([`Floor`]: by default `kbf_threads::disk::keep_free`, 5% of the file system and at
//! least 1 GiB, the rule every farm directory that evicts to a floor keeps) plus
//! the object's length, so a full disk refuses uploads instead of failing part-way
//! through them (and the Raft log, often on the same disk, keeps room). The store
//! deletes nothing on its own: a refusal wakes whoever waits on
//! [`FsStore::refused_full`] (the server's evictor, `kbf-front`'s `Cache::evict_round`),
//! which forgets the least recently used objects and deletes their files while the
//! free space is under twice the floor ([`FsStore::room`]).
//!
//! No Object Lock. I/O runs on tokio's blocking pool, so every call needs a tokio
//! runtime. An I/O failure is [`ObjectStoreError::Transport`], naming the operation
//! and the path.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use futures::StreamExt;
use kbf_threads::disk::{FreeSpace as _, Space, Statvfs, keep_free};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::body::{self, BodyStream};
use crate::types::Token;
use crate::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectInfo, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

/// The first bytes of every object file.
const MAGIC: &[u8; 8] = b"kbf-obj1";

/// The longest header: the magic, the key's length, and a key of 1024 bytes.
const MAX_HEADER: usize = MAGIC.len() + 2 + 1024;

/// How long the sorted keys of a listing are kept for the pages that continue it. A
/// later page reads every header again.
const LISTING_KEPT_FOR: Duration = Duration::from_secs(300);

/// How many keys one reading of every header keeps for a listing's pages: sixteen of
/// the largest pages. kbf's keys are under 100 bytes, so a few MiB; at most 16 MiB of
/// key text, at S3's 1024-byte limit.
const LISTING_WINDOW: usize = 16 * PageSize::MAX.get() as usize;

/// The id of the next listing that reads every header. Process-wide, so a token from
/// one store never names another store's kept listing.
static NEXT_LISTING: AtomicU64 = AtomicU64::new(0);

/// Why a store could not be opened on a directory.
#[derive(Debug, thiserror::Error)]
pub enum FsStoreError {
    /// A file system call failed.
    #[error("{op} {path}: {source}")]
    Io {
        /// What the store was doing.
        op: &'static str,
        /// The path it was doing it to.
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The directory's `LOCK` is held: another store is open on it.
    #[error("lock {path}: {source} (is another server using this directory?)")]
    Locked {
        /// The lock file.
        path: PathBuf,
        #[source]
        source: std::fs::TryLockError,
    },
}

/// The free space a store's writes must leave on its file system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Floor {
    /// [`keep_free`] of the file system's size: 5%, at least 1 GiB.
    KeepFree,
    /// This many bytes.
    Bytes(u64),
}

impl Floor {
    /// The floor in bytes, on a file system of `total` bytes.
    #[must_use]
    pub fn bytes(self, total: u64) -> u64 {
        match self {
            Self::KeepFree => keep_free(total),
            Self::Bytes(bytes) => bytes,
        }
    }
}

/// Where a store reads its file system's [`Space`] from, given its `objects/`
/// directory: [`statvfs_source`], or a test's fake ([`FsStore::open_with_space`]).
pub type SpaceSource = Arc<dyn Fn(&Path) -> io::Result<Space> + Send + Sync>;

/// The real file systems: `statvfs` ([`Statvfs`]).
#[must_use]
pub fn statvfs_source() -> SpaceSource {
    Arc::new(|objects: &Path| Statvfs.space(objects))
}

/// For tests of eviction only: a file system of `total` bytes that holds nothing but
/// the store's object files (every file under `objects/` counts, header included).
#[doc(hidden)]
#[must_use]
pub fn objects_source(total: u64) -> SpaceSource {
    Arc::new(move |objects: &Path| {
        let mut held = 0_u64;
        for shard in entries(objects)? {
            // A file deleted since the listing holds nothing.
            held += entries(&shard)?
                .iter()
                .map(|f| fs::metadata(f).map_or(0, |m| m.len()))
                .sum::<u64>();
        }
        Ok(Space {
            total,
            available: total.saturating_sub(held),
        })
    })
}

/// The store's free space against its floor ([`FsStore::room`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Room {
    /// The bytes still free on the file system.
    pub available: u64,
    /// The floor in bytes: a write that would leave less free is refused.
    pub floor: u64,
}

/// A bucket in a local directory (see the module docs). Conditional writes, no Object
/// Lock. A clone is another handle on the same open store: the directory stays locked
/// until the last handle is dropped.
#[derive(Clone)]
pub struct FsStore {
    inner: Arc<Inner>,
}

struct Inner {
    objects: PathBuf,
    /// The free space a write must leave on the file system.
    floor: Floor,
    /// Where the file system's size and free space come from.
    space: SpaceSource,
    /// Woken each time a write is refused for want of room.
    full: Notify,
    tmp: PathBuf,
    /// The name of the next temporary file.
    next: AtomicU64,
    /// The sorted keys of the last listing, for the pages that continue it.
    listing: Mutex<Option<Listing>>,
    /// How many keys a listing keeps ([`LISTING_WINDOW`]; smaller in tests).
    window: usize,
    /// How many times a listing read every header: once per listing, not per page.
    walks: AtomicU64,
    /// Held for the store's life: closing it releases the lock.
    _lock: File,
}

impl FsStore {
    /// Opens the store in `dir`, creating the directory and its layout if missing,
    /// locking it, and removing every file left in `tmp/`. A write is refused while the
    /// file system would be left with less than `floor` free.
    ///
    /// # Errors
    /// The directory cannot be created, locked or read, or a leftover cannot be removed.
    pub fn open(dir: &Path, floor: Floor) -> Result<Self, FsStoreError> {
        Self::open_with_space(dir, floor, statvfs_source())
    }

    /// [`FsStore::open`], with the file system's size and free space read from `space`
    /// instead of `statvfs`: for tests that need a file system of a given size.
    ///
    /// # Errors
    /// As [`FsStore::open`].
    #[doc(hidden)]
    pub fn open_with_space(
        dir: &Path,
        floor: Floor,
        space: SpaceSource,
    ) -> Result<Self, FsStoreError> {
        fs::create_dir_all(dir).map_err(open_failed("create", dir))?;
        let lock_path = dir.join("LOCK");
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(open_failed("open", &lock_path))?;
        lock.try_lock().map_err(|source| FsStoreError::Locked {
            path: lock_path.clone(),
            source,
        })?;
        let objects = dir.join("objects");
        for shard in 0..=u8::MAX {
            let path = objects.join(format!("{shard:02x}"));
            fs::create_dir_all(&path).map_err(open_failed("create", &path))?;
        }
        let tmp = dir.join("tmp");
        fs::create_dir_all(&tmp).map_err(open_failed("create", &tmp))?;
        for path in entries(&tmp).map_err(open_failed("list", &tmp))? {
            fs::remove_file(&path).map_err(open_failed("remove", &path))?;
        }
        for path in [tmp.as_path(), objects.as_path(), dir] {
            sync_dir(path).map_err(open_failed("sync", path))?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                objects,
                floor,
                space,
                full: Notify::new(),
                tmp,
                next: AtomicU64::new(0),
                listing: Mutex::new(None),
                window: LISTING_WINDOW,
                walks: AtomicU64::new(0),
                _lock: lock,
            }),
        })
    }
}

/// One object file [`FsStore::scan`] read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scanned {
    /// The key its header names.
    pub key: ObjectKey,
    /// The object's length: the file's, less the header.
    pub len: u64,
    /// When the file was last modified (written, for a file this store wrote).
    pub mtime: SystemTime,
}

/// What [`FsStore::scan`] found in one shard.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShardScan {
    /// Every file with an object header, in no particular order.
    pub objects: Vec<Scanned>,
    /// Files without one (too short to hold a header, or not starting with one): no
    /// key, so no object. A read of the key they were meant for answers `NotFound`.
    pub unreadable: usize,
}

impl ShardScan {
    /// How many files the scan opened.
    #[must_use]
    pub fn opened(&self) -> usize {
        self.objects.len() + self.unreadable
    }
}

impl FsStore {
    /// How many shards ([`FsStore::scan`]) the objects are spread over: one per value
    /// of a `u8`.
    pub const SHARDS: usize = 256;

    /// Reads the header of every object file in shard `shard` (`objects/<shard as two
    /// hex digits>/`): its key, its length and its modification time. Opens every
    /// file in the shard, reads no object's bytes.
    ///
    /// # Errors
    /// The shard cannot be listed, or a file in it cannot be opened or read.
    pub async fn scan(&self, shard: u8) -> Result<ShardScan, ObjectStoreError> {
        let inner = Arc::clone(&self.inner);
        blocking(move || inner.scan(shard)).await
    }

    /// Whether the file system has less free space than the store's floor: every write
    /// is refused until room is made.
    ///
    /// # Errors
    /// The file system cannot be asked (`statvfs`).
    pub fn under_floor(&self) -> Result<bool, ObjectStoreError> {
        let room = self.room()?;
        Ok(room.available < room.floor)
    }

    /// The free space on the store's file system, and its floor in bytes.
    ///
    /// # Errors
    /// The file system cannot be asked (`statvfs`).
    pub fn room(&self) -> Result<Room, ObjectStoreError> {
        self.inner.room()
    }

    /// Completes after a write is refused for want of room: at the next refusal, or
    /// at once if one came since the last wait completed (one wake-up is kept for
    /// a waiter that is not waiting yet).
    pub async fn refused_full(&self) {
        self.inner.full.notified().await;
    }
}

impl Inner {
    /// The directory and the file of `key`'s object.
    fn place(&self, key: &ObjectKey) -> (PathBuf, PathBuf) {
        let name = hex::encode(Sha256::digest(key.as_str().as_bytes()));
        let dir = self.objects.join(&name[..2]);
        let file = dir.join(name);
        (dir, file)
    }

    /// Writes `body` as `key`'s object (see the module docs). The temporary file is
    /// removed whether or not the object was linked.
    fn put(&self, key: &ObjectKey, body: &[u8]) -> Result<(), ObjectStoreError> {
        self.room_for(body.len() as u64)?;
        let tmp = self.tmp_path();
        let written = write_new(&tmp, key, body);
        self.publish(key, &tmp, written)
    }

    /// A new name in `tmp/`.
    fn tmp_path(&self) -> PathBuf {
        self.tmp
            .join(self.next.fetch_add(1, Ordering::Relaxed).to_string())
    }

    /// Links the synced file `tmp` under `key`'s name if `written` (its writing)
    /// succeeded, removes `tmp` either way, and syncs the name's directory.
    fn publish(
        &self,
        key: &ObjectKey,
        tmp: &Path,
        written: Result<(), ObjectStoreError>,
    ) -> Result<(), ObjectStoreError> {
        let (dir, path) = self.place(key);
        let linked = written.and_then(|()| link(tmp, &path, key));
        let removed = fs::remove_file(tmp);
        linked?;
        removed.map_err(failed("remove", tmp))?;
        sync_dir(&dir).map_err(failed("sync", &dir))
    }

    /// A file in `tmp/` for an object of `len` bytes, its header written: the start of
    /// a streamed write. Refused as `put` is when the floor leaves no room for `len`.
    fn begin(&self, key: &ObjectKey, len: u64) -> Result<(File, PathBuf), ObjectStoreError> {
        self.room_for(len)?;
        let tmp = self.tmp_path();
        let mut file = File::create_new(&tmp).map_err(failed("create", &tmp))?;
        // A failed write leaves the file in `tmp/`, which the next open empties.
        file.write_all(&header(key))
            .map_err(failed("write", &tmp))?;
        Ok((file, tmp))
    }

    /// The bytes free to an unprivileged writer on the store's file system, and the
    /// floor on it.
    fn room(&self) -> Result<Room, ObjectStoreError> {
        let space = (self.space)(&self.objects).map_err(failed("statvfs", &self.objects))?;
        Ok(Room {
            available: space.available,
            floor: self.floor.bytes(space.total),
        })
    }

    /// Refuses a write of `len` bytes that would leave less than the floor free, and
    /// wakes [`FsStore::refused_full`].
    fn room_for(&self, len: u64) -> Result<(), ObjectStoreError> {
        let Room { available, floor } = self.room()?;
        if available < floor.saturating_add(len) {
            self.full.notify_one();
            return Err(ObjectStoreError::Transport(Arc::new(Full {
                dir: self.objects.clone(),
                free: available,
                floor,
                len,
            })));
        }
        Ok(())
    }

    fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        let (_, path) = self.place(key);
        Ok(Object::open(&path)?.is_some_and(|o| o.key == *key))
    }

    fn get(&self, key: &ObjectKey, range: ByteRange) -> Result<Bytes, ObjectStoreError> {
        let (_, path) = self.place(key);
        let Some(object) = Object::open(&path)? else {
            return Err(ObjectStoreError::NotFound(key.clone()));
        };
        if object.key != *key {
            return Err(ObjectStoreError::Protocol(format!(
                "{} holds object {}, not {key}",
                path.display(),
                object.key
            )));
        }
        if range.offset() >= object.size {
            return Err(ObjectStoreError::InvalidRange {
                key: key.clone(),
                range,
            });
        }
        // At most the object's size, which is at most the file's.
        let len = (object.size.min(range.offset() + range.size()) - range.offset()) as usize;
        let mut bytes = vec![0; len];
        object
            .file
            .read_exact_at(&mut bytes, object.header + range.offset())
            .map_err(failed("read", &path))?;
        Ok(Bytes::from(bytes))
    }

    fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let (_, path) = self.place(key);
        match fs::remove_file(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            removed => removed.map_err(failed("remove", &path)),
        }
    }

    fn scan(&self, shard: u8) -> Result<ShardScan, ObjectStoreError> {
        let dir = self.objects.join(format!("{shard:02x}"));
        let mut scan = ShardScan::default();
        for path in entries(&dir).map_err(failed("list", &dir))? {
            match Object::open(&path)? {
                Some(object) => scan.objects.push(Scanned {
                    key: object.key,
                    len: object.size,
                    mtime: object.mtime,
                }),
                None => scan.unreadable += 1,
            }
        }
        Ok(scan)
    }

    /// The first `max` keys under `prefix` after `after`, in key order: cut from the
    /// kept keys of `listing` (the listing a page's token came from) while they last,
    /// or from a new window of keys, every header read again.
    fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ObjectKey>,
        listing: Option<u64>,
        max: usize,
    ) -> Result<ListPage, ObjectStoreError> {
        let start_in =
            |keys: &[ObjectInfo]| after.map_or(0, |a| keys.partition_point(|o| o.key <= *a));
        let kept = listing.and_then(|id| {
            let kept = self.listing.lock().unwrap_or_else(PoisonError::into_inner);
            kept.as_ref()
                .filter(|l| {
                    l.id == id && l.prefix == *prefix && l.taken.elapsed() < LISTING_KEPT_FOR
                })
                .map(|l| (l.id, Arc::clone(&l.keys), l.cut))
        });
        // A kept window whose keys this page has passed, with keys beyond it, is used
        // up: the page reads the next window.
        let kept = kept.filter(|(_, keys, cut)| !cut || start_in(keys) < keys.len());
        let (id, keys, cut) = match kept {
            Some(kept) => kept,
            None => {
                let (keys, cut) = self.walk(prefix, after)?;
                let id = NEXT_LISTING.fetch_add(1, Ordering::Relaxed);
                *self.listing.lock().unwrap_or_else(PoisonError::into_inner) = Some(Listing {
                    id,
                    prefix: prefix.clone(),
                    keys: Arc::clone(&keys),
                    cut,
                    taken: Instant::now(),
                });
                (id, keys, cut)
            }
        };
        let start = start_in(&keys);
        let end = keys.len().min(start.saturating_add(max));
        let objects = keys[start..end].to_vec();
        // The token names this listing and the last key of this page; the next page
        // starts after it.
        let next = (end < keys.len() || cut)
            .then(|| {
                objects.last().map(|last| {
                    ListToken(Token::Kept {
                        listing: id,
                        after: last.key.clone(),
                    })
                })
            })
            .flatten();
        if next.is_none() {
            // The listing is done: free its keys.
            let mut listing = self.listing.lock().unwrap_or_else(PoisonError::into_inner);
            listing.take_if(|l| l.id == id);
        }
        Ok(ListPage { objects, next })
    }

    /// Reads the key of every object and returns the first [`Inner::window`] of those
    /// under `prefix` and after `after`, in key order, and whether any were left out.
    fn walk(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ObjectKey>,
    ) -> Result<(Arc<[ObjectInfo]>, bool), ObjectStoreError> {
        self.walks.fetch_add(1, Ordering::Relaxed);
        let mut found = BTreeMap::new();
        let mut cut = false;
        for shard in entries(&self.objects).map_err(failed("list", &self.objects))? {
            for path in entries(&shard).map_err(failed("list", &shard))? {
                let Some(object) = Object::open(&path)? else {
                    continue;
                };
                if prefix.matches(&object.key) && after.is_none_or(|a| object.key > *a) {
                    found.insert(object.key, object.size);
                    // Never more than the window (and one key) held at once.
                    if found.len() > self.window {
                        found.pop_last();
                        cut = true;
                    }
                }
            }
        }
        let keys = found
            .into_iter()
            .map(|(key, size)| ObjectInfo { key, size })
            .collect();
        Ok((keys, cut))
    }
}

/// The sorted keys of one listing ([`Inner::list`]).
struct Listing {
    /// Named by the tokens of its pages.
    id: u64,
    prefix: KeyPrefix,
    keys: Arc<[ObjectInfo]>,
    /// Whether keys past the last of `keys` were left out (the window was full).
    cut: bool,
    taken: Instant,
}

/// An open object file and what its header says.
struct Object {
    file: File,
    key: ObjectKey,
    /// The header's length: where the object's bytes start.
    header: u64,
    /// The object's length.
    size: u64,
    /// The file's modification time.
    mtime: SystemTime,
}

impl Object {
    /// Opens the object file at `path`: `None` if there is none, or if it is too short
    /// to hold a header or does not start with one.
    ///
    /// # Errors
    /// The file cannot be opened or read.
    fn open(path: &Path) -> Result<Option<Self>, ObjectStoreError> {
        let file = match File::open(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            opened => opened.map_err(failed("open", path))?,
        };
        let stat = file.metadata().map_err(failed("stat", path))?;
        let mut head = [0; MAX_HEADER];
        let read = file.read_at(&mut head, 0).map_err(failed("read", path))?;
        Ok(parse_header(&head[..read]).map(|(key, header)| Self {
            file,
            key,
            header,
            // The header was read from the file, so it is no longer than it.
            size: stat.len() - header,
            // Linux and macOS keep a modification time on every file.
            mtime: stat.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        }))
    }
}

/// The header of `key`'s object file.
fn header(key: &ObjectKey) -> Vec<u8> {
    let key = key.as_str().as_bytes();
    // A key is at most 1024 bytes.
    let len = (key.len() as u16).to_be_bytes();
    [MAGIC.as_slice(), &len, key].concat()
}

/// The key and the header's length, if `bytes` start with a header.
fn parse_header(bytes: &[u8]) -> Option<(ObjectKey, u64)> {
    let rest = bytes.strip_prefix(MAGIC.as_slice())?;
    let (len, rest) = rest.split_first_chunk::<2>()?;
    let len = usize::from(u16::from_be_bytes(*len));
    let key = ObjectKey::new(std::str::from_utf8(rest.get(..len)?).ok()?).ok()?;
    Some((key, (MAGIC.len() + 2 + len) as u64))
}

/// Writes `key`'s header and `body` to a new file at `tmp`, and syncs it.
fn write_new(tmp: &Path, key: &ObjectKey, body: &[u8]) -> Result<(), ObjectStoreError> {
    let mut file = File::create_new(tmp).map_err(failed("create", tmp))?;
    file.write_all(&header(key)).map_err(failed("write", tmp))?;
    file.write_all(body).map_err(failed("write", tmp))?;
    file.sync_all().map_err(failed("sync", tmp))
}

/// Links `tmp` as `path`, which must not exist.
fn link(tmp: &Path, path: &Path, key: &ObjectKey) -> Result<(), ObjectStoreError> {
    match fs::hard_link(tmp, path) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            Err(ObjectStoreError::AlreadyExists(key.clone()))
        }
        linked => linked.map_err(failed("link", path)),
    }
}

/// The paths of the entries of `dir`.
fn entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    fs::read_dir(dir)?.map(|e| e.map(|e| e.path())).collect()
}

/// Syncs a directory, so the names created and removed in it are durable.
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// A write refused for lack of free space.
#[derive(Debug, thiserror::Error)]
#[error(
    "{dir}: {free} bytes free, under the floor of {floor} bytes plus the {len} bytes of \
     this write (the least recently used objects are being evicted to make room; try \
     again)"
)]
struct Full {
    dir: PathBuf,
    free: u64,
    floor: u64,
    len: u64,
}

/// An I/O failure of a store call: the operation and the path. The cause is its
/// source, which [`ObjectStoreError::Transport`] prints after it.
#[derive(Debug, thiserror::Error)]
#[error("{op} {path}")]
struct Failed {
    op: &'static str,
    path: PathBuf,
    #[source]
    source: io::Error,
}

fn failed<'a>(op: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> ObjectStoreError + 'a {
    move |source| {
        ObjectStoreError::Transport(Arc::new(Failed {
            op,
            path: path.to_owned(),
            source,
        }))
    }
}

fn open_failed<'a>(
    op: &'static str,
    path: &'a Path,
) -> impl FnOnce(io::Error) -> FsStoreError + 'a {
    move |source| FsStoreError::Io {
        op,
        path: path.to_owned(),
        source,
    }
}

/// Runs `f` on tokio's blocking pool.
async fn blocking<T, F>(f: F) -> Result<T, ObjectStoreError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ObjectStoreError> + Send + 'static,
{
    // A panic in `f` is a bug; it is raised again here, as if `f` had run inline.
    tokio::task::spawn_blocking(f)
        .await
        .expect("a store task panicked")
}

impl ObjectStore for FsStore {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            conditional_put: true,
            object_lock: false,
        }
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        if retain_until.is_some() {
            return Err(ObjectStoreError::Unsupported("Object Lock retention"));
        }
        let (inner, key) = (Arc::clone(&self.inner), key.clone());
        blocking(move || inner.put(&key, &body)).await
    }

    /// Writes each piece to the object's file in `tmp/` as it comes (on the blocking
    /// pool, one piece at a time), then syncs and links it as [`ObjectStore::put_new`]
    /// does. A failed or wrong-length body removes the file. A write cut short by its
    /// caller going away leaves the file in `tmp/`, which the next open empties.
    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        mut body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        let (inner, k) = (Arc::clone(&self.inner), key.clone());
        let (file, tmp) = blocking(move || inner.begin(&k, len)).await?;
        let mut written = 0u64;
        let filled = async {
            let mut file = file;
            while let Some(piece) = body.next().await {
                let piece = piece?;
                written += piece.len() as u64;
                if written > len {
                    return Err(body::too_long(len));
                }
                let path = tmp.clone();
                file = blocking(move || {
                    let mut file = file;
                    file.write_all(&piece).map_err(failed("write", &path))?;
                    Ok(file)
                })
                .await?;
            }
            if written < len {
                return Err(body::too_short(len, written));
            }
            Ok(file)
        }
        .await;
        let (inner, key) = (Arc::clone(&self.inner), key.clone());
        blocking(move || {
            let synced = filled.and_then(|file| file.sync_all().map_err(failed("sync", &tmp)));
            inner.publish(&key, &tmp, synced)
        })
        .await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let (inner, key) = (Arc::clone(&self.inner), key.clone());
        blocking(move || inner.get(&key, range)).await
    }

    async fn may_hold(&self, key: &ObjectKey) -> Result<bool, ObjectStoreError> {
        let (inner, key) = (Arc::clone(&self.inner), key.clone());
        blocking(move || inner.may_hold(&key)).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let (inner, key) = (Arc::clone(&self.inner), key.clone());
        blocking(move || inner.delete(&key)).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        let (inner, prefix) = (Arc::clone(&self.inner), prefix.clone());
        let listing = after.and_then(ListToken::listing);
        let after = after.map(ListToken::local_after).transpose()?.cloned();
        let max = usize::from(max_keys.get());
        blocking(move || inner.list(&prefix, after.as_ref(), listing, max)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> ObjectKey {
        ObjectKey::new(name).expect("a valid key")
    }

    async fn list_all(store: &FsStore, prefix: &KeyPrefix) -> Vec<String> {
        let mut keys = Vec::new();
        let mut after = None;
        loop {
            let page = store
                .list(prefix, after.as_ref(), PageSize::new(2).expect("size"))
                .await
                .expect("list");
            keys.extend(page.objects.into_iter().map(|o| o.key.as_str().to_owned()));
            match page.next {
                Some(t) => after = Some(t),
                None => return keys,
            }
        }
    }

    /// Catches a listing that reads every header again for each page (a rebuild over
    /// a directory of N objects would open N files per page, N squared over the
    /// listing): 7 keys in pages of 2 take one walk. A listing interleaved with one of
    /// another prefix walks again rather than cutting its pages from the other's keys,
    /// a token of a listing no longer kept or used under another prefix walks again,
    /// and a listing that ended keeps nothing for the next.
    #[tokio::test]
    async fn a_paged_listing_reads_the_headers_once() {
        let dir = std::env::temp_dir().join(format!("kbf-objstore-walks.{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = FsStore::open(&dir, Floor::Bytes(1)).expect("open");
        for i in 0..7 {
            for p in ["a", "b"] {
                let k = key(&format!("{p}/{i}"));
                store
                    .put_new(&k, Bytes::from_static(b"x"), None)
                    .await
                    .expect("put");
            }
        }
        let walks = || store.inner.walks.load(Ordering::Relaxed);
        let a = KeyPrefix::new("a/").expect("prefix");
        let b = KeyPrefix::new("b/").expect("prefix");
        let want: Vec<String> = (0..7).map(|i| format!("a/{i}")).collect();
        assert_eq!(list_all(&store, &a).await, want);
        assert_eq!(walks(), 1);
        assert!(store.inner.listing.lock().expect("lock").is_none());

        let size = PageSize::new(2).expect("size");
        let first = store.list(&a, None, size).await.expect("list");
        let other = store.list(&b, None, size).await.expect("list");
        assert_eq!(other.objects[0].key.as_str(), "b/0");
        let second = store
            .list(&a, first.next.as_ref(), size)
            .await
            .expect("list");
        let keys: Vec<&str> = second.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["a/2", "a/3"]);
        assert_eq!(walks(), 4);

        // A token of a listing no longer kept (here, of a's first walk) walks again,
        // and so does a token of the kept listing used under another prefix.
        let stale = store
            .list(&a, first.next.as_ref(), size)
            .await
            .expect("list");
        let keys: Vec<&str> = stale.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["a/2", "a/3"]);
        assert_eq!(walks(), 5);
        let elsewhere = store
            .list(&b, stale.next.as_ref(), size)
            .await
            .expect("list");
        let keys: Vec<&str> = elsewhere.objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["b/0", "b/1"]);
        assert_eq!(walks(), 6);
        drop(store);
        fs::remove_dir_all(&dir).expect("remove");
    }

    /// Catches a listing that keeps every key under its prefix (memory growing with the
    /// prefix, not the window), one that ends at the end of its first window, and one
    /// that cuts the next page from a used-up window instead of reading the next: 7
    /// keys through a window of 3 in pages of 2 come back whole, in order, never more
    /// than 3 kept, in 3 walks. Pages that stay within a window walk no more. A file in
    /// a shard directory that is not an object is skipped by every walk.
    #[tokio::test]
    async fn a_listing_longer_than_its_window_keeps_one_window_at_a_time() {
        let dir = std::env::temp_dir().join(format!("kbf-objstore-window.{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut store = FsStore::open(&dir, Floor::Bytes(1)).expect("open");
        Arc::get_mut(&mut store.inner).expect("one handle").window = 3;
        for i in 0..7 {
            for p in ["a", "b"] {
                let k = key(&format!("{p}/{i}"));
                store
                    .put_new(&k, Bytes::from_static(b"x"), None)
                    .await
                    .expect("put");
            }
        }
        let shard = store.inner.objects.join("00");
        fs::create_dir_all(&shard).expect("shard");
        fs::write(shard.join("foreign"), b"not an object").expect("plant");
        let a = KeyPrefix::new("a/").expect("prefix");
        let size = PageSize::new(2).expect("size");
        let (mut got, mut after, mut most) = (Vec::new(), None, 0);
        loop {
            let page = store.list(&a, after.as_ref(), size).await.expect("list");
            if let Some(kept) = store.inner.listing.lock().expect("lock").as_ref() {
                most = most.max(kept.keys.len());
            }
            got.extend(page.objects.into_iter().map(|o| o.key.as_str().to_owned()));
            match page.next {
                Some(t) => after = Some(t),
                None => break,
            }
        }
        let want: Vec<String> = (0..7).map(|i| format!("a/{i}")).collect();
        assert_eq!(got, want);
        assert_eq!(most, 3, "the most keys a listing kept");
        assert_eq!(store.inner.walks.load(Ordering::Relaxed), 3);
        assert!(store.inner.listing.lock().expect("lock").is_none());
        drop(store);
        fs::remove_dir_all(&dir).expect("remove");
    }

    /// Catches a fresh listing [`ListToken::after`] a key that walks from the start of
    /// the prefix (with more keys before the resume key than the window holds, the
    /// window is used up before it and the listing ends empty), or that starts at the
    /// key: resumed after a/2 and after a/5 through a window of 3, in pages of 1 and 2,
    /// the listing is exactly the keys after it, in order.
    #[tokio::test]
    async fn a_file_store_listing_resumed_past_its_window_lists_only_the_rest() {
        let dir = std::env::temp_dir().join(format!("kbf-objstore-resume.{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut store = FsStore::open(&dir, Floor::Bytes(1)).expect("open");
        Arc::get_mut(&mut store.inner).expect("one handle").window = 3;
        for i in 0..9 {
            let k = key(&format!("a/{i}"));
            store
                .put_new(&k, Bytes::from_static(b"x"), None)
                .await
                .expect("put");
        }
        let a = KeyPrefix::new("a/").expect("prefix");
        for (from, first) in [("a/2", 3), ("a/5", 6)] {
            for size in [1, 2] {
                let size = PageSize::new(size).expect("size");
                let (mut got, mut after) = (Vec::new(), Some(ListToken::after(&key(from))));
                loop {
                    let page = store.list(&a, after.as_ref(), size).await.expect("list");
                    got.extend(page.objects.into_iter().map(|o| o.key.as_str().to_owned()));
                    // A listing that repeats a key would otherwise never end.
                    assert!(got.len() <= 9, "after {from}: a key listed twice: {got:?}");
                    match page.next {
                        Some(t) => after = Some(t),
                        None => break,
                    }
                }
                let want: Vec<String> = (first..9).map(|i| format!("a/{i}")).collect();
                assert_eq!(got, want, "after {from} in pages of {}", size.get());
            }
        }
        drop(store);
        fs::remove_dir_all(&dir).expect("remove");
    }
}
