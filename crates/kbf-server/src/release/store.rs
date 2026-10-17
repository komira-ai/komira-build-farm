//! The release store: a directory of immutable files.
//!
//! ```text
//! <dir>/files/<sha256>                          a tarball, named by the digest of its bytes
//! <dir>/index/<commit>/<platform>/<name>        "<sha256> <size>\n": what a component is
//! <dir>/index/<commit>/<platform>/<name>.attestation    its build attestation bundle
//! ```
//!
//! - **Immutable per key.** A key (commit, platform, component) names the same bytes for
//!   good: a second write of the same bytes changes nothing, of other bytes is
//!   [`PutError::Conflict`]. An entry is created with a hard link, which fails when the
//!   name exists, so two writers racing for one key cannot both win.
//! - **No visible half write.** Every file is written under a `*.partial` name next to
//!   its destination, synced, and only then moved into place (a rename for a tarball, a
//!   link for an entry), and the directory is synced. Nothing reads a `.partial`: a tarball
//!   is found by a 64-digit digest, an entry by the exact names above. A write that fails
//!   or is dropped removes its partial; one a power cut left stays, unseen and unserved.
//! - **Paths are typed.** Every path is built from a [`Key`] or a [`Sha256Hex`], never from
//!   request text ([`super::key`]).
//! - **A tarball is stored as it came.** Nothing here opens it or checks an attestation:
//!   the host that installs a release verifies it (`docs/design/pull-updates.md`).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use futures::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;

use kbf_types::ReleasePlatform;

use super::key::{ATTESTATION_SUFFIX, Commit, Component, Key, Sha256Hex};

/// The largest tarball the store takes: 512 MiB.
pub const MAX_TARBALL_BYTES: u64 = 512 << 20;

/// The largest attestation bundle the store takes: 256 KiB.
pub const MAX_ATTESTATION_BYTES: u64 = 256 << 10;

/// How large a file may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// A tarball, in bytes.
    pub tarball: u64,
    /// An attestation bundle, in bytes.
    pub attestation: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            tarball: MAX_TARBALL_BYTES,
            attestation: MAX_ATTESTATION_BYTES,
        }
    }
}

/// A request body, as the store reads it: chunks, or the error that cut the body.
pub type Body<'a> = &'a mut (dyn Stream<Item = io::Result<Bytes>> + Unpin + Send);

/// Why a file is not stored.
#[derive(Debug, thiserror::Error)]
pub enum PutError {
    /// The key already holds other bytes.
    #[error("{0} already holds other bytes: a release is immutable")]
    Conflict(String),
    /// The body is longer than the limit.
    #[error("the body is longer than the limit of {limit} bytes")]
    TooLarge { limit: u64 },
    /// The body has no bytes.
    #[error("the body is empty")]
    Empty,
    /// The body could not be read to its end (the client went away or sent garbage).
    #[error("reading the body: {0}")]
    Body(#[source] io::Error),
    /// The store's directory failed.
    #[error("the release store: {0}")]
    Disk(#[source] io::Error),
}

/// What a put did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    /// False when the key already held exactly these bytes.
    pub created: bool,
    /// The digest of the bytes (of a tarball; of an attestation, its own).
    pub sha256: Sha256Hex,
    pub size: u64,
}

/// A stored tarball.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentInfo {
    pub component: Component,
    pub sha256: Sha256Hex,
    pub size: u64,
    /// Whether the attestation bundle is stored too.
    pub attested: bool,
}

/// The components stored for one platform of one commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlatformListing {
    pub platform: ReleasePlatform,
    pub components: Vec<ComponentInfo>,
    /// Every component is stored with its attestation.
    pub complete: bool,
}

/// What is stored for one commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitListing {
    pub commit: Commit,
    pub platforms: Vec<PlatformListing>,
}

/// The store in a directory.
#[derive(Debug)]
pub struct ReleaseStore {
    files: PathBuf,
    index: PathBuf,
    limits: Limits,
}

static PARTIALS: AtomicU64 = AtomicU64::new(0);

/// A `.partial` file's path, removed when this is dropped unless it was moved into place.
struct Partial {
    path: PathBuf,
    placed: bool,
}

impl Partial {
    /// A new, empty, uniquely named `.partial` in `dir`.
    async fn create(dir: &Path) -> io::Result<(File, Self)> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let n = PARTIALS.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("{}-{nanos}-{n}.partial", std::process::id()));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        let partial = Self {
            path,
            placed: false,
        };
        Ok((file, partial))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// The file is where it belongs now.
    fn placed(mut self) {
        self.placed = true;
    }
}

impl Drop for Partial {
    fn drop(&mut self) {
        if !self.placed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// What publishing a small file found.
enum Published {
    Created,
    Same,
    Differs,
}

async fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Makes the creation of a name in `dir` durable.
async fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir).await?.sync_all().await
}

/// `<sha256> <size>\n` read back; `None` for anything else.
fn parse_entry(text: &[u8]) -> Option<(Sha256Hex, u64)> {
    let text = std::str::from_utf8(text).ok()?;
    let (sha, size) = text.strip_suffix('\n')?.split_once(' ')?;
    Some((Sha256Hex::parse(sha)?, size.parse().ok()?))
}

fn corrupt(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{} is not a release entry", path.display()),
    )
}

impl ReleaseStore {
    /// The store in `dir`, which is created with its two subdirectories if missing.
    ///
    /// # Errors
    /// A directory cannot be created.
    pub fn open(dir: &Path, limits: Limits) -> io::Result<Self> {
        let (files, index) = (dir.join("files"), dir.join("index"));
        std::fs::create_dir_all(&files)?;
        std::fs::create_dir_all(&index)?;
        Ok(Self {
            files,
            index,
            limits,
        })
    }

    #[must_use]
    pub fn limits(&self) -> Limits {
        self.limits
    }

    fn platform_dir(&self, key: &Key) -> PathBuf {
        self.index
            .join(key.commit.as_str())
            .join(key.platform.as_str())
    }

    fn entry_path(&self, key: &Key) -> PathBuf {
        self.platform_dir(key).join(key.component.as_str())
    }

    fn attestation_path(&self, key: &Key) -> PathBuf {
        let name = format!("{}{ATTESTATION_SUFFIX}", key.component);
        self.platform_dir(key).join(name)
    }

    /// Creates `dir` and the directories above it that are missing, and syncs the parent
    /// of each one created, so that a file made in them is still reachable after a power
    /// cut (a synced file under an unsynced directory can vanish with it).
    async fn make_dirs(dir: &Path) -> io::Result<()> {
        let mut created = Vec::new();
        let mut at = dir;
        while !tokio::fs::try_exists(at).await? {
            created.push(at);
            at = at.parent().unwrap_or(at);
        }
        tokio::fs::create_dir_all(dir).await?;
        for new in created {
            sync_dir(new.parent().unwrap_or(new)).await?;
        }
        Ok(())
    }

    /// Creates `path` holding `content` unless it exists; says which happened.
    async fn publish(path: &Path, content: &[u8]) -> io::Result<Published> {
        let dir = path.parent().unwrap_or(Path::new(""));
        Self::make_dirs(dir).await?;
        let (mut file, partial) = Partial::create(dir).await?;
        file.write_all(content).await?;
        file.sync_all().await?;
        // A link fails when the name exists (a writer got there first); then what is
        // there decides. With nothing there the link's own error is the answer.
        let Err(refused) = tokio::fs::hard_link(partial.path(), path).await else {
            sync_dir(dir).await?;
            return Ok(Published::Created);
        };
        let held = tokio::fs::read(path).await.map_err(|_| refused)?;
        Ok(if held == content {
            Published::Same
        } else {
            Published::Differs
        })
    }

    /// `publish`, with another content refused as a conflict on `key`.
    async fn publish_for(key: &Key, path: &Path, content: &[u8]) -> Result<bool, PutError> {
        match Self::publish(path, content).await.map_err(PutError::Disk)? {
            Published::Created => Ok(true),
            Published::Same => Ok(false),
            Published::Differs => Err(PutError::Conflict(key.to_string())),
        }
    }

    /// The tarball entry of `key`, if stored.
    async fn entry(&self, key: &Key) -> io::Result<Option<(Sha256Hex, u64)>> {
        let path = self.entry_path(key);
        let Some(text) = read_optional(&path).await? else {
            return Ok(None);
        };
        parse_entry(&text).map(Some).ok_or_else(|| corrupt(&path))
    }

    /// Stores the tarball `body` as `key`.
    ///
    /// The bytes are written to a `.partial` while their digest is computed, synced and
    /// renamed to their digest; then the entry is linked. The key already holding these
    /// bytes is not an error and changes nothing.
    ///
    /// # Errors
    /// [`PutError`]: other bytes already stored, a body past the limit or empty or cut,
    /// or the directory failing. A failed put leaves nothing visible behind.
    pub async fn put_tarball(&self, key: &Key, body: Body<'_>) -> Result<Stored, PutError> {
        let (mut file, partial) = Partial::create(&self.files).await.map_err(PutError::Disk)?;
        let (mut hasher, mut size) = (Sha256::new(), 0_u64);
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(PutError::Body)?;
            size += chunk.len() as u64;
            if size > self.limits.tarball {
                return Err(PutError::TooLarge {
                    limit: self.limits.tarball,
                });
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(PutError::Disk)?;
        }
        if size == 0 {
            return Err(PutError::Empty);
        }
        file.sync_all().await.map_err(PutError::Disk)?;
        drop(file);
        let sha256 = Sha256Hex::of_digest(&hasher.finalize());
        let line = format!("{sha256} {size}\n");
        // Before the rename: other bytes under this key must leave no file behind.
        if let Some((held, _)) = self.entry(key).await.map_err(PutError::Disk)?
            && held != sha256
        {
            return Err(PutError::Conflict(key.to_string()));
        }
        let to = self.files.join(sha256.as_str());
        tokio::fs::rename(partial.path(), &to)
            .await
            .map_err(PutError::Disk)?;
        partial.placed();
        sync_dir(&self.files).await.map_err(PutError::Disk)?;
        let created = Self::publish_for(key, &self.entry_path(key), line.as_bytes()).await?;
        Ok(Stored {
            created,
            sha256,
            size,
        })
    }

    /// Stores the attestation bundle `body` for `key`, whether or not its tarball is
    /// stored yet.
    ///
    /// # Errors
    /// As [`ReleaseStore::put_tarball`].
    pub async fn put_attestation(&self, key: &Key, body: Body<'_>) -> Result<Stored, PutError> {
        let mut bundle = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(PutError::Body)?;
            if (bundle.len() + chunk.len()) as u64 > self.limits.attestation {
                return Err(PutError::TooLarge {
                    limit: self.limits.attestation,
                });
            }
            bundle.extend_from_slice(&chunk);
        }
        if bundle.is_empty() {
            return Err(PutError::Empty);
        }
        let created = Self::publish_for(key, &self.attestation_path(key), &bundle).await?;
        Ok(Stored {
            created,
            sha256: Sha256Hex::of_digest(&Sha256::digest(&bundle)),
            size: bundle.len() as u64,
        })
    }

    /// The stored tarball of `key`.
    ///
    /// # Errors
    /// The directory fails, or the entry is not one.
    pub async fn component(&self, key: &Key) -> io::Result<Option<ComponentInfo>> {
        let Some((sha256, size)) = self.entry(key).await? else {
            return Ok(None);
        };
        let attested = tokio::fs::try_exists(self.attestation_path(key)).await?;
        Ok(Some(ComponentInfo {
            component: key.component,
            sha256,
            size,
            attested,
        }))
    }

    /// The attestation bundle of `key`, for an offer.
    ///
    /// # Errors
    /// The directory fails.
    pub async fn attestation(&self, key: &Key) -> io::Result<Option<Vec<u8>>> {
        read_optional(&self.attestation_path(key)).await
    }

    /// The stored tarball whose digest is `sha256`, open.
    ///
    /// # Errors
    /// The directory fails.
    pub async fn open_tarball(&self, sha256: &Sha256Hex) -> io::Result<Option<File>> {
        match File::open(self.files.join(sha256.as_str())).await {
            Ok(file) => Ok(Some(file)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Everything stored, by commit (sorted) and platform ([`ReleasePlatform::ALL`]
    /// order). A commit or platform with no stored tarball is left out. Only the names the
    /// store writes are looked at: anything else in the directory is ignored.
    ///
    /// # Errors
    /// The directory fails.
    pub async fn list(&self) -> io::Result<Vec<CommitListing>> {
        let mut commits = Vec::new();
        let mut dir = tokio::fs::read_dir(&self.index).await?;
        while let Some(entry) = dir.next_entry().await? {
            if let Some(commit) = entry.file_name().to_str().and_then(Commit::parse) {
                commits.push(commit);
            }
        }
        commits.sort();
        let mut listing = Vec::new();
        for commit in commits {
            let mut platforms = Vec::new();
            for platform in ReleasePlatform::ALL {
                let mut components = Vec::new();
                for component in Component::ALL {
                    let key = Key {
                        commit: commit.clone(),
                        platform,
                        component,
                    };
                    components.extend(self.component(&key).await?);
                }
                if !components.is_empty() {
                    // Complete: every tarball this platform's hosts need is stored, attested.
                    let complete = platform.components().iter().all(|name| {
                        components
                            .iter()
                            .any(|c| c.component.as_str() == *name && c.attested)
                    });
                    platforms.push(PlatformListing {
                        platform,
                        components,
                        complete,
                    });
                }
            }
            if !platforms.is_empty() {
                listing.push(CommitListing { commit, platforms });
            }
        }
        Ok(listing)
    }
}
