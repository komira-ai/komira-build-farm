//! Whether the two halves keep one cache (`kbf_storage::local_cache`), in either mode
//! (`kbf all-in-one`'s server half, `kbf storage-and-daemon`'s storage half): the
//! daemon's input cache and that half's hot store share a disk, so the input cache's
//! directory and index are the node's only local copies of blobs, the hot store keeps no
//! flushed copies beside them, and one budget and one fill count both.
//!
//! Only when the hot store is a directory on the filesystem of `--data-dir`: with no hot
//! store, local storage keeps nothing on this disk but the cold store (the durable store,
//! not a cache), and a hot store on another disk is a copy on another disk. Either way
//! each half then keeps what it keeps without the other, and the start says why.

use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::sync::Arc;

use kbf_server::Args;
use kbf_server::stores::StoreUri;
use kbf_storage::local_cache::LocalCache;

/// Gives both halves (`server`, the server or storage half's flags, and `daemon`) the
/// one cache when the hot store shares the data directory's disk; the cache, or `None`
/// (each half as without the other).
pub(crate) fn share(server: &mut Args, daemon: &mut kbf_node::Cli) -> Option<Arc<LocalCache>> {
    let data = daemon.data_dir();
    let Some(StoreUri::File(hot)) = &server.hot_store else {
        tracing::info!("one cache: off (no hot store directory)");
        return None;
    };
    if !same_disk(hot, &data) {
        let (hot, data) = (hot.display(), data.display());
        tracing::info!("one cache: off ({hot} is not on the filesystem of {data})");
        return None;
    }
    let local = Arc::new(LocalCache::new());
    server.local_cache = Some(Arc::clone(&local));
    daemon.share_cache(Arc::clone(&local));
    let (hot, data) = (hot.display(), data.display());
    tracing::info!("one cache: {data}/cache, with the hot store {hot} on its filesystem");
    Some(local)
}

/// Whether `a` and `b` are on one filesystem, each judged by its nearest existing
/// directory (neither need exist yet).
fn same_disk(a: &Path, b: &Path) -> bool {
    matches!((device(a), device(b)), (Some(a), Some(b)) if a == b)
}

/// The device of `path`, or of its nearest ancestor that exists.
fn device(path: &Path) -> Option<u64> {
    path.ancestors()
        .find_map(|at| std::fs::metadata(at).ok())
        .map(|found| found.dev())
}

/// For the modes' tests: whether `daemon` holds `local` itself, not a cache of its own
/// nor none. Marks `local` (its storage bytes read 4242), so call it once per cache.
#[cfg(test)]
pub(crate) fn daemon_holds(local: &LocalCache, daemon: &kbf_node::Cli) -> bool {
    assert!(local.attach_storage(Box::new(|| 4242)), "marked before");
    format!("{daemon:?}")
        .contains("local_cache: Some(LocalCache { fill: Fill { inputs: 0, storage: 4242 } })")
}

/// For the modes' tests: a directory under the test binary's (unit tests get no
/// `CARGO_TARGET_TMPDIR`), not made: the check judges a path by its nearest existing
/// directory.
#[cfg(test)]
pub(crate) fn under_target(name: &str) -> std::path::PathBuf {
    std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("kbf-all-in-one-unit")
        .join(format!("{name}-{}", std::process::id()))
}

#[cfg(test)]
#[path = "one_cache_tests.rs"]
mod tests;
