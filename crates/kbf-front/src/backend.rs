//! [`Backend`]: what Execute and the scheduler need of the store, and nothing more. Three
//! implementations: a [`Cache`] in this process (what `kbf-server` and `kbf-all-in-one`
//! have always run), a [`Storage`] reached over gRPC (`kbf frontend` without members,
//! which holds no blobs of its own: `docs/design/architecture-v2.md`, sections 2 and 7),
//! and the storage nodes of a map ([`Backend::routed`], `kbf frontend` with members),
//! which is the second until the map has two members.
//!
//! The calls are the ones the scheduler side makes: the action-cache lookup before a
//! run, the Action and Command blobs, the check that every input is held, and the two
//! halves of the action-cache write after one. Client CAS, ByteStream and action-cache
//! calls are not here: they are the cache's services, or the proxy's.
//!
//! **The input check over a [`Storage`]** ([`walk`]) reads each Directory of the tree
//! from the node and asks it which files are missing. It remembers nothing between
//! Executes: the verified-subtree memory of [`Cache::walk_inputs`] is the node's to keep
//! (a call that does the check there is the next step, and a [`Storage`] method). Over a
//! map the same walk reads each Directory from its owners.
//!
//! **The action-cache write over a map.** A node checks an entry against the blobs it
//! holds itself, and a result's blobs are on the owners of their own digests. The first
//! half ([`Backend::prepare_action_result`]) therefore asks the farm: the closure is
//! worked out (the trees read from their owners) and every blob in it must be held
//! somewhere, and the result is stored where the map puts it. The second half copies the
//! entry's blobs to the action's owner before it commits there
//! (`kbf_route::Router::commit_with_closure`), until av2-12 gives an entry the
//! acknowledgements of its writers.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::{StreamExt as _, TryStreamExt as _};
use kbf_meta::{ActionRecord, ActionWriteError, Role};
use kbf_objstore::ObjectStore;
use kbf_proto::reapi::ActionResult;
use kbf_route::Router;
use kbf_storage::{Storage, StorageError};
use kbf_types::Digest;
use prost::Message as _;

use crate::cache::Cache;
use crate::closure;
use crate::input_tree::{Walked, decode};
use crate::meta_log::{Leadership, MetaLog};
use crate::metrics::CacheMetrics;
use crate::routed::Routed;

/// The most Directory blobs [`walk`] reads from a node at once.
const DIRS_IN_FLIGHT: usize = 16;

/// The most digests one FindMissing of [`walk`] names: a message well under the gRPC
/// limit, with a call per few thousand files of a tree.
const FILES_PER_ASK: usize = 4096;

/// The store behind Execute and the scheduler. Cheap to clone.
#[derive(Clone)]
pub struct Backend(Arc<dyn Plane>);

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Backend(..)")
    }
}

/// What a store does for [`Backend`]: object safe, so Execute and the farm are not
/// generic over the metadata log and object store of a cache they may not have.
trait Plane: Send + Sync {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>>;
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>>;
    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> BoxFuture<'a, Answer<Option<(Digest, ActionResult)>>>;
    fn prepare_action_result<'a>(
        &'a self,
        result: &'a ActionResult,
    ) -> BoxFuture<'a, Answer<ActionRecord>>;
    fn commit_action_record(
        &self,
        action: Digest,
        record: ActionRecord,
    ) -> BoxFuture<'_, Answer<()>>;
    fn walk_inputs(&self, root: Digest) -> BoxFuture<'_, Answer<Walked>>;
    fn metrics(&self) -> &CacheMetrics;
    fn render_metrics(&self) -> String;
    fn leadership(&self) -> Option<Leadership>;
}

type Answer<T> = Result<T, StorageError>;

impl Backend {
    /// A store of a storage node (or any [`Storage`]) reached through `storage`, with
    /// metrics of its own: the frontend holds no cache to keep them.
    #[must_use]
    pub fn remote(storage: Arc<dyn Storage>) -> Self {
        Self(Arc::new(Remote {
            storage,
            metrics: CacheMetrics::default(),
        }))
    }

    /// A store of the storage nodes the map `routed` names, once it has two members or
    /// more ([`Routed::active`]); until then, the node `storage` reaches, as
    /// [`Backend::remote`] does.
    ///
    /// The action-cache write is the router's ([`Router::commit_with_closure`]): the
    /// result and the closure are checked against the owners of every blob in them, the
    /// result blob is stored where the map puts it, and the entry is committed at the
    /// action's owner with its blobs copied there first.
    #[must_use]
    pub fn routed(routed: Arc<Routed>, storage: Arc<dyn Storage>) -> Self {
        Self(Arc::new(RoutedPlane {
            routed,
            bootstrap: Remote {
                storage,
                metrics: CacheMetrics::default(),
            },
        }))
    }

    /// The digests among `digests` the store does not hold ([`Storage::find_missing`]).
    ///
    /// # Errors
    /// The store could not answer.
    pub async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, StorageError> {
        self.0.find_missing(digests).await
    }

    pub(crate) async fn read_blob(&self, digest: &Digest) -> Result<bytes::Bytes, StorageError> {
        self.0.read_blob(digest).await
    }

    pub(crate) async fn action_result(
        &self,
        action: &Digest,
    ) -> Result<Option<(Digest, ActionResult)>, StorageError> {
        self.0.action_result(action).await
    }

    /// The first half of an action-cache write ([`Storage::prepare_action_result`]).
    ///
    /// # Errors
    /// As the store answers.
    pub async fn prepare_action_result(
        &self,
        result: &ActionResult,
    ) -> Result<ActionRecord, StorageError> {
        self.0.prepare_action_result(result).await
    }

    /// The second half, as a daemon's result ([`Storage::commit_action_record`]).
    ///
    /// # Errors
    /// As the store answers.
    pub async fn commit_action_record(
        &self,
        action: Digest,
        record: ActionRecord,
    ) -> Result<(), StorageError> {
        self.0.commit_action_record(action, record).await
    }

    pub(crate) async fn walk_inputs(&self, root: Digest) -> Result<Walked, StorageError> {
        self.0.walk_inputs(root).await
    }

    /// What the service counts: the cache's own metrics, or this frontend's.
    #[must_use]
    pub fn metrics(&self) -> &CacheMetrics {
        self.0.metrics()
    }

    /// [`Backend::metrics`] in the Prometheus text format (the cache adds its store's).
    #[must_use]
    pub fn render_metrics(&self) -> String {
        self.0.render_metrics()
    }

    /// The id and term of a durable metadata log the store has ([`MetaLog::leadership`]);
    /// none for a store without one.
    #[must_use]
    pub fn leadership(&self) -> Option<Leadership> {
        self.0.leadership()
    }
}

impl<M: MetaLog, O: ObjectStore + 'static> From<Arc<Cache<M, O>>> for Backend {
    fn from(cache: Arc<Cache<M, O>>) -> Self {
        Self(Arc::new(Local(cache)))
    }
}

/// A cache in this process.
struct Local<M, O>(Arc<Cache<M, O>>);

impl<M: MetaLog, O: ObjectStore + 'static> Plane for Local<M, O> {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        Box::pin(async move {
            self.0
                .find_missing(digests)
                .await
                .map_err(StorageError::from)
        })
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>> {
        Box::pin(async move { self.0.read_blob(digest).await.map_err(StorageError::from) })
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> BoxFuture<'a, Answer<Option<(Digest, ActionResult)>>> {
        Box::pin(async move {
            self.0
                .action_result(action)
                .await
                .map_err(StorageError::from)
        })
    }

    fn prepare_action_result<'a>(
        &'a self,
        result: &'a ActionResult,
    ) -> BoxFuture<'a, Answer<ActionRecord>> {
        Box::pin(async move {
            self.0
                .prepare_action_result(result)
                .await
                .map_err(StorageError::from)
        })
    }

    fn commit_action_record(
        &self,
        action: Digest,
        record: ActionRecord,
    ) -> BoxFuture<'_, Answer<()>> {
        Box::pin(async move {
            self.0
                .commit_action_record(Role::Daemon, action, record)
                .await
                .map_err(StorageError::from)
        })
    }

    fn walk_inputs(&self, root: Digest) -> BoxFuture<'_, Answer<Walked>> {
        Box::pin(async move { self.0.walk_inputs(root).await.map_err(StorageError::from) })
    }

    fn metrics(&self) -> &CacheMetrics {
        self.0.metrics()
    }

    fn render_metrics(&self) -> String {
        self.0.render_metrics()
    }

    fn leadership(&self) -> Option<Leadership> {
        self.0.meta().leadership()
    }
}

/// A [`Storage`], and the metrics of the service that calls it.
struct Remote {
    storage: Arc<dyn Storage>,
    metrics: CacheMetrics,
}

impl Plane for Remote {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        self.storage.find_missing(digests)
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>> {
        self.storage.read_blob(digest)
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> BoxFuture<'a, Answer<Option<(Digest, ActionResult)>>> {
        self.storage.action_result(action)
    }

    fn prepare_action_result<'a>(
        &'a self,
        result: &'a ActionResult,
    ) -> BoxFuture<'a, Answer<ActionRecord>> {
        self.storage.prepare_action_result(result)
    }

    fn commit_action_record(
        &self,
        action: Digest,
        record: ActionRecord,
    ) -> BoxFuture<'_, Answer<()>> {
        self.storage
            .commit_action_record(Role::Daemon, action, record)
    }

    fn walk_inputs(&self, root: Digest) -> BoxFuture<'_, Answer<Walked>> {
        Box::pin(async move { walk(&Held(&*self.storage), root).await })
    }

    fn metrics(&self) -> &CacheMetrics {
        &self.metrics
    }

    fn render_metrics(&self) -> String {
        self.metrics.render()
    }

    fn leadership(&self) -> Option<Leadership> {
        None
    }
}

/// The storage nodes of a map, or the one node `--storage` names until the map has two
/// members: [`Remote`] when [`Routed::active`] is false.
struct RoutedPlane {
    routed: Arc<Routed>,
    bootstrap: Remote,
}

impl RoutedPlane {
    /// The router, when the map is to be routed by.
    fn routing(&self) -> Option<&Router> {
        if self.routed.active() {
            return Some(self.routed.router());
        }
        self.routed.tag_bootstrap();
        None
    }
}

impl Plane for RoutedPlane {
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        match self.routing() {
            Some(router) => Box::pin(router.find_missing(digests)),
            None => self.bootstrap.find_missing(digests),
        }
    }

    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>> {
        match self.routing() {
            Some(router) => Box::pin(router.read_blob(digest)),
            None => self.bootstrap.read_blob(digest),
        }
    }

    fn action_result<'a>(
        &'a self,
        action: &'a Digest,
    ) -> BoxFuture<'a, Answer<Option<(Digest, ActionResult)>>> {
        match self.routing() {
            Some(router) => Box::pin(router.action_result(action)),
            None => self.bootstrap.action_result(action),
        }
    }

    fn prepare_action_result<'a>(
        &'a self,
        result: &'a ActionResult,
    ) -> BoxFuture<'a, Answer<ActionRecord>> {
        match self.routing() {
            Some(router) => Box::pin(prepare(router, result)),
            None => self.bootstrap.prepare_action_result(result),
        }
    }

    fn commit_action_record(
        &self,
        action: Digest,
        record: ActionRecord,
    ) -> BoxFuture<'_, Answer<()>> {
        match self.routing() {
            Some(router) => Box::pin(router.commit_with_closure(Role::Daemon, action, record)),
            None => self.bootstrap.commit_action_record(action, record),
        }
    }

    fn walk_inputs(&self, root: Digest) -> BoxFuture<'_, Answer<Walked>> {
        match self.routing() {
            Some(router) => Box::pin(async move { walk(router, root).await }),
            None => self.bootstrap.walk_inputs(root),
        }
    }

    fn metrics(&self) -> &CacheMetrics {
        self.bootstrap.metrics()
    }

    fn render_metrics(&self) -> String {
        self.bootstrap.render_metrics()
    }

    fn leadership(&self) -> Option<Leadership> {
        None
    }
}

/// The first half of an action-cache write over the owners: works out the closure of
/// `result` (reading its output trees from wherever the map has them), asks which of its
/// blobs the farm does not hold, and stores `result` as a blob where the map puts it. As
/// [`Cache::prepare_action_result`] does for one node's blobs.
///
/// # Errors
/// [`ActionWriteError::Absent`] naming every blob of the closure no owner holds; a
/// missing tree, a malformed result, or owners that could not answer.
async fn prepare(router: &Router, result: &ActionResult) -> Answer<ActionRecord> {
    let (mut closure, trees) = closure::direct(result)?;
    for tree in trees {
        closure::add_tree(&mut closure, &tree, router.read_blob(&tree).await?)?;
    }
    let needed: Vec<Digest> = closure.iter().copied().collect();
    let missing = router.find_missing(&needed).await?;
    let absent = held_by_none(router, &missing).await?;
    if !absent.is_empty() {
        return Err(ActionWriteError::Absent(absent).into());
    }
    let stored = router
        .put(bytes::Bytes::from(result.encode_to_vec()))
        .await?;
    Ok(ActionRecord {
        result: stored,
        closure,
    })
}

/// Those of `missing`, which the digests' primaries lack, that no owner holds. A
/// map change moves a blob's primary before the blob is copied there, so a primary that
/// lacks it is not the farm lacking it: the read order, which ends with the previous
/// generation's owners, is asked for the first byte. (Unlike a missing input, which the
/// client uploads again, a missing output would fail the action.)
///
/// # Errors
/// An owner of a digest could not say, and none had it.
async fn held_by_none(router: &Router, missing: &[Digest]) -> Answer<Vec<Digest>> {
    let mut absent = Vec::new();
    for digest in missing {
        match router.read_range(digest, 0, 1).await {
            Ok(_) => {}
            Err(StorageError::NotFound(_)) => absent.push(*digest),
            Err(e) => return Err(e),
        }
    }
    Ok(absent)
}

/// What the input check reads: a blob, and which of some files are held.
trait Blobs: Send + Sync {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>>;
    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>>;
}

/// One storage node's blobs.
struct Held<'a>(&'a dyn Storage);

impl Blobs for Held<'_> {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>> {
        self.0.read_blob(digest)
    }

    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        self.0.find_missing(digests)
    }
}

/// The blobs of a farm, routed by its map.
impl Blobs for Router {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<bytes::Bytes>> {
        Box::pin(Router::read_blob(self, digest))
    }

    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        Box::pin(Router::find_missing(self, digests))
    }
}

/// The input check over a [`Storage`]: every Directory under `root` is read (a level at
/// a time, [`DIRS_IN_FLIGHT`] at once), and the files they name are asked about in
/// batches. A Directory the node does not hold, or holds but cannot read, is reported
/// absent and nothing under it is looked at; the report lists absent directories
/// (sorted) before absent files (sorted), as [`Cache::walk_inputs`] does.
///
/// # Errors
/// The node could not answer, or a Directory it holds does not decode.
async fn walk(storage: &dyn Blobs, root: Digest) -> Answer<Walked> {
    let (mut seen, mut dirs_read) = (BTreeSet::from([root]), 0);
    let (mut absent_dirs, mut files) = (BTreeSet::new(), BTreeSet::new());
    let mut level = vec![root];
    while !level.is_empty() {
        let read: Vec<_> = futures::stream::iter(level)
            .map(|dir| async move { (dir, storage.read_blob(&dir).await) })
            .buffer_unordered(DIRS_IN_FLIGHT)
            .collect()
            .await;
        level = Vec::new();
        for (dir, blob) in read {
            match blob {
                Err(StorageError::NotFound(_) | StorageError::Unreachable(_)) => {
                    absent_dirs.insert(dir);
                }
                other => {
                    dirs_read += 1;
                    let decoded = decode(&dir, &other?)?;
                    files.extend(decoded.files.iter().copied());
                    level.extend(decoded.dirs.iter().copied().filter(|d| seen.insert(*d)));
                }
            }
        }
    }
    let files: Vec<Digest> = files.into_iter().collect();
    let asked: Vec<Vec<Digest>> = futures::stream::iter(files.chunks(FILES_PER_ASK))
        .then(|chunk| storage.find_missing(chunk))
        .try_collect()
        .await?;
    let absent_files: BTreeSet<Digest> = asked.into_iter().flatten().collect();
    Ok(Walked {
        absent: absent_dirs.into_iter().chain(absent_files).collect(),
        dirs_read,
    })
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
