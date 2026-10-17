//! The storage half of `kbf storage-and-daemon` (`docs/design/architecture-v2.md`,
//! sections 1 and 10): the storage node `kbf storage` runs, on the same flags
//! ([`StorageFlags`]), with the storage it serves also handed to the caller in-process
//! ([`StorageHalf::storage`]), and the service that serves it callable in-process
//! ([`StorageHalf::in_process`]), so the daemon half of the same process reads and writes
//! it through calls, with no socket between.
//!
//! [`start`] opens the stores and binds the listener as
//! [`crate::storage_node::start_storage`] does, with two differences:
//! - **One cache.** When `flags.server.local_cache` is set (the daemon half's input
//!   cache shares the hot store's disk), the write tier keeps no flushed copies and its
//!   bytes are counted in that one cache's budget, as `kbf all-in-one`'s server half
//!   does (`kbf_storage::local_cache`).
//! - **The handle.** The [`Storage`] the listener serves, and the service over it
//!   ([`kbf_storage::service::InProcess`]), are kept, for the daemon half.
//!
//! - **The map and the chain.** With `--frontend` the half binds its listener, registers
//!   (retrying until the frontend answers), opens its cache as the registration's member
//!   and boot (its objects are named as `kbf storage`'s are), and heartbeats as
//!   `kbf storage` does. It holds the maps the frontend sends in a
//!   [`kbf_mapsync::MapHolder`] ([`StorageHalf::maps`]), and its service judges every call
//!   by that map, serving nothing until the first arrives (UNAVAILABLE `no storage map
//!   yet`); under mutual TLS it is a link of the chain that copies a write to the blob's
//!   second owner, with the repair worker that makes a copy owed
//!   ([`StorageHalf::repairs`]); all as [`crate::replication`] says. The check is the
//!   service's own, made on every call, so the daemon half, which calls the service
//!   in-process ([`StorageHalf::in_process`]), is judged by the same map and its writes
//!   are copied by the same chain as a call over the listener.
//!
//! The rest (the listener's checks, the stores, the metadata log, the flusher and the
//! start-up pass over a `file://` cold store) is `kbf storage`'s, step for step.

use std::error::Error;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{CacheStorage, RebuildConfig, SegmentsConfig};
use kbf_mapsync::MapHolder;
use kbf_meta::Retention;
use kbf_objstore::{FsStore, ObjectStore};
use kbf_storage::Storage;
use kbf_storage::repair::Repairs;
use kbf_storage::service::InProcess;
use kbf_threads::disk::Statvfs;

use crate::rebuild::Background;
use crate::replication::{REPAIR_DIR, bind_node};
use crate::serve::{ServeError, StorageListener, bind_storage_socket};
use crate::start::{AbortOnDrop, open_cold_file, share_storage, tier_bytes};
use crate::startup_pass::StartPass;
use crate::storage_node::{StorageFlags, open_cache};
use crate::stores::{FILE_PREFIX_ROOT, StoreUri, Stores};
use crate::write_tier::FLUSH_EVERY;
use crate::{RaftMetaLog, SERVER_VERSION, s3_store};

/// A started storage half: bound, its write tier attached and its flusher (and any
/// start-up pass) running, not yet serving.
pub struct StorageHalf {
    /// Where the listener is.
    pub addr: SocketAddr,
    storage: Arc<dyn Storage>,
    in_process: InProcess,
    start_line: String,
    serving: Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>,
    maps: Arc<MapHolder>,
    repairs: Option<Arc<Repairs>>,
    _flusher: Option<AbortOnDrop>,
    _background: Background,
    _joiner: Option<AbortOnDrop>,
    _repair: Option<AbortOnDrop>,
}

impl StorageHalf {
    /// The storage map the node holds ([`crate::storage_node::StorageHandle::maps`]).
    #[must_use]
    pub fn maps(&self) -> Arc<MapHolder> {
        Arc::clone(&self.maps)
    }

    /// The copies this node owes: its repair journal. `None` for a node that does not
    /// replicate (no `--frontend`, or one that admits callers in plain text).
    #[must_use]
    pub fn repairs(&self) -> Option<Arc<Repairs>> {
        self.repairs.clone()
    }

    /// The storage the listener serves, itself, with none of the service's checks: for a
    /// daemon half that is not part of a farm ([`kbf_storage::service::InProcess`] is the
    /// view that is).
    #[must_use]
    pub fn storage(&self) -> Arc<dyn Storage> {
        Arc::clone(&self.storage)
    }

    /// The storage service the listener serves, called in-process: what the daemon half
    /// reaches the node that is its own member through, so that a write it makes takes the
    /// path a call over the listener takes (the generation check, the chain's forward to
    /// the replica).
    #[must_use]
    pub fn in_process(&self) -> InProcess {
        self.in_process.clone()
    }

    /// The line the node prints once it is bound, as `kbf storage` prints it:
    /// `kbf-storage <version> storage=<addr> log=<id>`.
    #[must_use]
    pub fn start_line(&self) -> &str {
        &self.start_line
    }

    /// Serves until the shutdown future given to [`start`] completes and the calls in
    /// flight end; the flusher and the pass stop then.
    ///
    /// # Errors
    /// The listener fails while serving.
    pub async fn serve(self) -> Result<(), ServeError> {
        let Self {
            serving,
            _flusher,
            _background,
            _joiner,
            _repair,
            ..
        } = self;
        serving.await
    }
}

/// Opens the stores `flags` name and binds the storage listener over the cache, as
/// `kbf storage` does, keeping the storage for the caller (the module docs).
///
/// # Errors
/// What [`crate::storage_node::start_storage`] refuses: the listener's flags (checked
/// first, before anything is opened), a store or log that cannot be opened, a listener
/// that cannot be bound, a write tier that refuses to start, or a `shutdown` that fails.
pub async fn start<S, F>(flags: &StorageFlags, shutdown: S) -> Result<StorageHalf, Box<dyn Error>>
where
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    let listener = flags.listener()?;
    let stores = flags.server.stores(&Statvfs)?;
    for line in stores.describe() {
        tracing::info!("{line}");
    }
    match &stores.cold {
        StoreUri::File(dir) => {
            let store = open_cold_file(&flags.server, dir)?;
            let files = Some(store.clone());
            let opened = (store, files);
            over(flags, listener, &stores, opened, FILE_PREFIX_ROOT, shutdown).await
        }
        StoreUri::S3(uri) => {
            let store = s3_store(uri, |name| std::env::var(name).ok())?;
            over(
                flags,
                listener,
                &stores,
                (store, None),
                &uri.prefix,
                shutdown,
            )
            .await
        }
    }
}

/// [`start`] over the object store the cold store's URI named: the metadata log, the
/// cache, its write tier, flusher and pass, and the listener over them.
async fn over<O, S, F>(
    flags: &StorageFlags,
    listener: StorageListener,
    stores: &Stores,
    (store, files): (O, Option<FsStore>),
    prefix_root: &str,
    shutdown: S,
) -> Result<StorageHalf, Box<dyn Error>>
where
    O: ObjectStore + 'static,
    S: FnOnce() -> io::Result<F>,
    F: Future<Output = ()> + Send + 'static,
{
    let args = &flags.server;
    let meta = RaftMetaLog::open_dir(&stores.meta_dir, Retention::default(), prefix_root).await?;
    let (prefix, log) = (meta.store_prefix(), meta.log_id());
    let grace = Duration::from_millis(args.cold_store_evict_grace_ms);
    let pass = files.map(|files| StartPass::over_raft(files, grace));
    // As `kbf storage` does ([`crate::storage_node::open_cache`]): bind, register with the
    // frontend if there is one, and only then open the cache, as the registration's member
    // and boot.
    let socket = bind_storage_socket(listener.listen)?;
    let (cache, registered) = open_cache(flags, socket, (meta, store, prefix, log)).await?;
    let cache = cache.with_writes(args.write_config());
    let local = args.local_cache.as_ref();
    let hot = stores.hot.as_ref();
    let (cache, tier) = crate::write_tier::attach_with(cache, hot, local.is_none()).await?;
    let cache = Arc::new(cache);
    share_storage(local, tier_bytes(Arc::downgrade(&cache)));
    let _flusher = tier.map(|_| {
        let flusher = crate::write_tier::run_flusher(Arc::clone(&cache), FLUSH_EVERY);
        AbortOnDrop(tokio::spawn(flusher))
    });
    let _background =
        crate::rebuild::spawn(&cache, &registered.catch_up, pass, RebuildConfig::default());
    let segments = SegmentsConfig {
        max_bytes: args.segment_upload_max_bytes,
        idle: Duration::from_secs(args.segment_upload_idle_secs),
    };
    let storage: Arc<dyn Storage> = Arc::new(CacheStorage::new(cache, segments));
    // After the log is open: it refuses a directory that holds files it did not write.
    let repair_dir = stores.meta_dir.join(REPAIR_DIR);
    let stop = Box::pin(shutdown()?);
    let served = Arc::clone(&storage);
    let node = bind_node(flags, listener, registered, served, &repair_dir, stop)?;
    let addr = node.addr;
    Ok(StorageHalf {
        addr,
        storage,
        in_process: node.in_process,
        start_line: format!("kbf-storage {SERVER_VERSION} storage={addr} log={log}"),
        serving: node.serving,
        maps: node.maps,
        repairs: node.repairs,
        _flusher,
        _background,
        _joiner: node.joiner,
        _repair: node.repair,
    })
}
