//! The per-node image store of the OCI backend: each image digest fetched from its
//! registry and unpacked once into `<images>/<hex>/rootfs`, read-only afterwards, shared
//! by every action that names it.
//!
//! **Acquisition is the node's own and digest-verified end to end** (`kbf-image`): the
//! manifest is fetched by the pinned digest and must hash to it (and must be one image,
//! not an index); the config and every layer blob are hashed as they are written and
//! must match the manifest. No container tool takes part. Then a helper container
//! ([`helper::unpack_job`]) applies the layers with `kbf-daemon __unpack-layers` inside a
//! user namespace that has the same mapping as an action's, so every file ends up owned
//! by the host id the action sees as the image's own id (root is the first subordinate
//! id), with no shifting pass. After that an action needs one `stat` of
//! `<hex>/rootfs`.
//!
//! The first use of an image pays for the fetch and the unpack, on the lease's prepare
//! step or, for the images the daemon is given with `--image`, at start; a second use
//! (and a second daemon on the same data directory) finds the directory and fetches
//! nothing.
//!
//! The unpack runs in a user namespace with the daemon user unmapped, the stage directory
//! the only writable path and the downloaded blobs read-only, so a hostile tar can write
//! nothing the daemon user owns.
//!
//! Single flight per digest: concurrent first uses of one image wait on one fetch and
//! unpack; a use cancelled midway leaves another to retry (a stage directory with a
//! different random name each time; the leftovers are removed by the sweep at the next
//! start).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::OnceCell;

use super::{JobError, Oci, at};
use crate::image::{ImageCheckError, ImageRef};

/// The directory of the rootfs under an image's directory.
pub(crate) use crate::unpack_layers::ROOTFS;

/// The prefix of an unpack in progress.
pub(crate) const STAGE_PREFIX: &str = ".stage-";

/// The per-node store.
#[derive(Debug)]
pub(crate) struct Images {
    dir: PathBuf,
    flights: Mutex<HashMap<String, Arc<OnceCell<()>>>>,
}

/// The hex of an image's `sha256:<hex>` digest, the name of its directory.
fn hex_of(image: &ImageRef) -> String {
    image
        .digest()
        .strip_prefix("sha256:")
        .unwrap_or(image.digest())
        .to_owned()
}

impl Images {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            flights: Mutex::new(HashMap::new()),
        }
    }

    /// The root filesystem of the image whose digest is `digest` (`sha256:<hex>`).
    pub(crate) fn rootfs_of(&self, digest: &str) -> PathBuf {
        self.dir
            .join(digest.strip_prefix("sha256:").unwrap_or(digest))
            .join(ROOTFS)
    }

    /// Whether `image` is unpacked.
    pub(crate) fn has(&self, image: &ImageRef) -> bool {
        self.rootfs_of(image.digest()).is_dir()
    }

    /// Fetches and unpacks `image` unless it is, waiting for a fetch already under way.
    ///
    /// # Errors
    /// The registry cannot be reached or refuses, the digest names an index or a
    /// manifest or blob that does not hash to its digest, or the layers cannot be
    /// unpacked.
    pub(crate) async fn ensure(&self, oci: &Oci, image: &ImageRef) -> Result<(), ImageCheckError> {
        if self.has(image) {
            return Ok(());
        }
        let flight = {
            let mut flights = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(flights.entry(hex_of(image)).or_default())
        };
        flight.get_or_try_init(|| self.acquire(oci, image)).await?;
        Ok(())
    }

    /// Fetches the layers into a directory of blobs, unpacks them into a stage directory,
    /// then renames the stage to the image's directory: presence of that name means
    /// complete. The blobs and the stage are removed whatever happens.
    async fn acquire(&self, oci: &Oci, image: &ImageRef) -> Result<(), ImageCheckError> {
        let hex = hex_of(image);
        let id = unique();
        let stage = self.dir.join(format!("{STAGE_PREFIX}{hex}-{id}"));
        let blobs = self.dir.join(format!("{STAGE_PREFIX}{hex}-{id}-blobs"));
        at(&self.dir, tokio::fs::create_dir_all(&self.dir).await).map_err(failed)?;
        // Searched by the container's ids on the way to an image's root, whatever umask
        // the daemon runs under.
        let open = std::fs::Permissions::from_mode(0o755);
        at(&self.dir, tokio::fs::set_permissions(&self.dir, open).await).map_err(failed)?;
        let made = self.fetch_and_unpack(oci, image, &stage, &blobs).await;
        // The blobs are the daemon's own files.
        let _ = tokio::fs::remove_dir_all(&blobs).await;
        if made.is_err() && stage.exists() {
            // The stage holds files only the container's ids own.
            let _ = oci.remove_dir_async(&stage).await;
        }
        made?;
        // The stage was open to the container's root; the image directory is not.
        let closed = std::fs::Permissions::from_mode(0o755);
        at(&stage, tokio::fs::set_permissions(&stage, closed).await).map_err(failed)?;
        let finished = self.dir.join(&hex);
        at(&finished, tokio::fs::rename(&stage, &finished).await).map_err(failed)
    }

    /// Downloads the image's layers into `blobs` (a directory the container's ids can
    /// read), makes `stage` (open to the container's root, which is not the daemon user:
    /// it makes the image's root inside, owned by itself) and unpacks the layers into it.
    async fn fetch_and_unpack(
        &self,
        oci: &Oci,
        image: &ImageRef,
        stage: &Path,
        blobs: &Path,
    ) -> Result<(), ImageCheckError> {
        at(blobs, tokio::fs::create_dir(blobs).await).map_err(failed)?;
        let open = std::fs::Permissions::from_mode(0o755);
        at(blobs, tokio::fs::set_permissions(blobs, open).await).map_err(failed)?;
        let layers = oci
            .registry()
            .fetch(image, blobs)
            .await
            .map_err(|e| match e {
                kbf_image::Error::Index(_) => ImageCheckError::Index(image.clone()),
                other => ImageCheckError::Failed(other.to_string()),
            })?;
        let mut names = Vec::with_capacity(layers.len());
        for layer in layers {
            let readable = std::fs::Permissions::from_mode(0o644);
            let path = blobs.join(&layer.name);
            at(&path, tokio::fs::set_permissions(&path, readable).await).map_err(failed)?;
            names.push(layer.name);
        }
        at(stage, tokio::fs::create_dir(stage).await).map_err(failed)?;
        let open = std::fs::Permissions::from_mode(0o777);
        at(stage, tokio::fs::set_permissions(stage, open).await).map_err(failed)?;
        oci.unpack_layers(stage, blobs, &names)
            .await
            .map_err(failed)
    }

    /// Removes what an unpack that was killed left: the stage directories and blobs.
    /// Blocking; the files in a stage are owned by subordinate ids, so it needs `oci`.
    ///
    /// # Errors
    /// What could not be removed, in words that name the sweep.
    pub(crate) fn sweep(&self, oci: &Oci) -> Result<(), String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Ok(());
        };
        let mut errors = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with(STAGE_PREFIX)
            {
                continue;
            }
            let removed = if path.is_dir() {
                oci.remove_dir_blocking(&path)
            } else {
                at(&path, std::fs::remove_file(&path))
            };
            errors.extend(removed.err().map(|e| e.to_string()));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "removing unpacks a previous daemon left: {}",
                errors.join("; ")
            ))
        }
    }
}

/// A step of the fetch or the unpack that failed, as the check of an image reports it.
fn failed(error: JobError) -> ImageCheckError {
    ImageCheckError::Failed(error.to_string())
}

/// A name part unique to this process and call.
fn unique() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
