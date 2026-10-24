//! A store over a hot and a cold backend.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::{Mutex as TurnLock, Notify};
use tokio::task::JoinHandle;

use crate::state::State;
use crate::{Backend, ByteRange, Error, Listed, Meta};

/// The shortest interval the background task waits between passes.
const MIN_RETRY: Duration = Duration::from_millis(1);

/// What one [`Tiered::write_back`] pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct WriteBackReport {
    /// Objects the cold tier now holds: copied there, or found there already with the same
    /// size and content tag.
    pub copied: Vec<String>,
    /// Objects the cold tier holds with a different size or content tag. They stay in the
    /// hot tier, are never evicted and are left alone by later passes until they are deleted.
    pub clashed: Vec<String>,
    /// Objects that could not be copied, with why. They stay pending and are tried again by
    /// the next pass.
    pub failed: Vec<(String, Error)>,
}

/// What one [`Tiered::evict`] pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct EvictionReport {
    /// Objects removed from the hot tier because the cold tier holds them.
    pub evicted: Vec<String>,
    /// Objects the cold tier turned out not to hold (gone, or other content): they are
    /// pending again, and stay in the hot tier.
    pub unconfirmed: Vec<String>,
    /// Objects that could not be checked or removed, with why. They stay in the hot tier.
    pub failed: Vec<(String, Error)>,
}

/// What one [`Tiered::maintain`] pass did: the write-back, then the eviction.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Maintenance {
    /// The copies to the cold tier.
    pub write_back: WriteBackReport,
    /// The removals from the hot tier.
    pub eviction: EvictionReport,
}

/// A store over a HOT and a COLD [`Backend`] that is itself a [`Backend`].
///
/// - **Put** goes to the hot tier, create-only, and succeeds when the hot tier has the
///   object. The cold tier is not involved, so a put succeeds while the cold tier is down.
///   Create-only is the hot tier's: a key evicted from it can be put again, and the
///   write-back then finds the old object in the cold tier and compares content tags (see
///   [`WriteBackReport::clashed`]). A put refused as existing for an object the store does
///   not count (an earlier put of it failed after the write landed) starts counting it, so
///   the write-back copies it.
/// - **Write-back** copies each object from the hot tier to the cold tier, create-only on
///   the cold side: a cold object already there with the same size and content tag counts as
///   copied. [`Tiered::write_back`] is one pass; [`Tiered::spawn_background`] runs passes.
///   An object is read whole into memory, one at a time, and a delete waits for the copy in
///   progress, so the backends must bound each of their calls with a timeout.
/// - **Reads** go to the hot tier, and to the cold tier only when the hot tier has no such
///   object (any other hot error is returned), with ranges. A read from the hot tier makes
///   the object the most recently used. Nothing is promoted from cold to hot.
/// - **Eviction** removes an object from the hot tier only when the cold tier holds it: a
///   HEAD of both tiers must give the same size and content tag. It runs while the hot tier
///   holds more bytes than its budget, least recently used first. With the cold tier down
///   nothing is evicted and the hot tier grows past its budget until the cold tier is back
///   and the write-back has run. [`Tiered::evict`] is one pass.
/// - **Delete** removes the object from both tiers, hot first, and an object is not copied
///   or evicted while it is being deleted.
/// - **List** is the union of both tiers without duplicates, the hot tier's size where both
///   hold the key. It lists the hot tier first: an object that is no longer in the hot tier
///   when it is listed was removed only after the cold tier held it, so the cold listing that
///   follows has it. It fails if either tier does not answer, so a listing that is used for
///   recovery or collection is never short.
///
/// # Restart
/// The write-back needs no database of its own. What is pending is what the hot tier holds
/// and the cold tier does not (or holds at another size), which [`Tiered::open`] reads by
/// listing both tiers. A store opened over the same backends as a stopped one therefore
/// finishes the stopped one's write-backs. What a restart loses is the use order: objects
/// found by the listing start as used in listing order.
///
/// One `Tiered` is the only writer of the hot tier it is opened over: it counts the hot
/// tier's bytes itself.
pub struct Tiered {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    budget: u64,
    state: Mutex<State>,
    /// Held while a pass handles one object and while an object is deleted, so that a delete
    /// cannot be undone by a copy that read the object before it.
    turn: TurnLock<()>,
    /// Woken by a put, so the background task copies it without waiting out its interval.
    wake: Notify,
}

impl Tiered {
    /// Opens a store over `hot` and `cold` that keeps the hot tier at or under
    /// `hot_budget` bytes by evicting objects the cold tier holds.
    ///
    /// Lists both tiers to find what the hot tier holds and which of it the cold tier holds
    /// with the same size; the rest is pending write-back.
    ///
    /// # Errors
    /// The error of a listing that failed.
    pub async fn open(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        hot_budget: u64,
    ) -> Result<Self, Error> {
        let in_hot = hot.list("").await?;
        let in_cold = cold.list("").await?;
        let cold_sizes: BTreeMap<String, u64> = in_cold
            .into_iter()
            .map(|object| (object.key, object.size))
            .collect();
        let mut state = State::default();
        for object in in_hot {
            let copied = cold_sizes.get(&object.key) == Some(&object.size);
            state.insert(&object.key, object.size, copied);
        }
        Ok(Self {
            hot,
            cold,
            budget: hot_budget,
            state: Mutex::new(state),
            turn: TurnLock::new(()),
            wake: Notify::new(),
        })
    }

    /// The bytes the store counts in the hot tier.
    #[must_use]
    pub fn hot_bytes(&self) -> u64 {
        self.state().bytes()
    }

    /// The objects the cold tier does not hold yet and that did not clash, oldest first.
    #[must_use]
    pub fn pending(&self) -> Vec<String> {
        self.state().pending()
    }

    /// The objects the cold tier holds with other content, in key order.
    #[must_use]
    pub fn clashed(&self) -> Vec<String> {
        self.state().clashed()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // No method panics while holding the lock, so a poisoned state is still whole.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether both tiers hold the key with the same size and content tag.
    async fn same_object(&self, key: &str) -> Result<bool, Error> {
        let hot = self.hot.head(key).await?;
        match self.cold.head(key).await {
            Ok(cold) => Ok(cold == hot),
            Err(Error::NotFound(_)) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Copies one object to the cold tier: `true` if the cold tier holds it afterwards,
    /// `false` if it holds other content under the key.
    async fn copy_one(&self, key: &str) -> Result<bool, Error> {
        let body = self.hot.get(key, None).await?;
        match self.cold.put(key, body).await {
            Ok(()) => Ok(true),
            Err(Error::AlreadyExists(_)) => self.same_object(key).await,
            Err(error) => Err(error),
        }
    }

    /// One write-back pass: copies the objects that are pending when it starts to the cold
    /// tier, oldest first.
    ///
    /// An object put during the pass waits for the next one, so a steady stream of puts
    /// cannot keep the pass from ending (the eviction after it would not run). An object
    /// deleted before its turn is left out. An object that fails stays pending and is tried
    /// again by the next pass. A pass ends at the first failure that says the cold tier (or
    /// the hot tier) is unavailable, since the next object would fail the same way.
    pub async fn write_back(&self) -> WriteBackReport {
        let mut report = WriteBackReport::default();
        let work = self.state().pending();
        for key in work {
            let _turn = self.turn.lock().await;
            if !self.state().is_pending(&key) {
                continue;
            }
            match self.copy_one(&key).await {
                Ok(true) => {
                    self.state().set_copied(&key, true);
                    report.copied.push(key);
                }
                Ok(false) => {
                    self.state().set_clashed(&key);
                    report.clashed.push(key);
                }
                Err(error) => {
                    if note_failure(&mut report.failed, key, error) {
                        break;
                    }
                }
            }
        }
        report
    }

    /// Starts tracking an object the hot tier holds and the store does not know: one whose
    /// put was refused as existing after an earlier put of it failed with the write landed.
    /// Left untracked it would never be copied to the cold tier.
    async fn adopt(&self, key: &str) {
        if self.state().tracks(key) {
            return;
        }
        if let Ok(meta) = self.hot.head(key).await {
            self.state().insert(key, meta.size, false);
            self.wake.notify_one();
        }
    }

    /// Removes one object from the hot tier if the cold tier holds it: `true` if removed,
    /// `false` if the cold tier does not hold it (it is pending again).
    async fn evict_one(&self, key: &str) -> Result<bool, Error> {
        if !self.same_object(key).await? {
            self.state().set_copied(key, false);
            return Ok(false);
        }
        self.hot.delete(key).await?;
        self.state().remove(key);
        Ok(true)
    }

    /// One eviction pass: while the hot tier holds more bytes than its budget, removes the
    /// least recently used object the cold tier holds.
    ///
    /// Objects the cold tier has not been seen to hold are never candidates, so with the
    /// cold tier down, or the write-back behind, the pass ends with the hot tier over its
    /// budget. Before each removal the cold tier is asked again; an object it no longer
    /// holds stays and becomes pending. A pass ends at the first failure that says a tier
    /// is unavailable.
    pub async fn evict(&self) -> EvictionReport {
        let mut report = EvictionReport::default();
        let mut skip = BTreeSet::new();
        loop {
            let _turn = self.turn.lock().await;
            let next = self.state().next_victim(self.budget, &skip);
            let Some(key) = next else {
                break;
            };
            match self.evict_one(&key).await {
                Ok(true) => report.evicted.push(key),
                Ok(false) => report.unconfirmed.push(key),
                Err(error) => {
                    skip.insert(key.clone());
                    if note_failure(&mut report.failed, key, error) {
                        break;
                    }
                }
            }
        }
        report
    }

    /// A write-back pass, then an eviction pass.
    pub async fn maintain(&self) -> Maintenance {
        let write_back = self.write_back().await;
        let eviction = self.evict().await;
        Maintenance {
            write_back,
            eviction,
        }
    }

    /// Starts a task that runs [`Tiered::maintain`] at once, then again whenever an object
    /// is put and at least every `retry_every` (a millisecond at least, so a zero interval
    /// does not keep the task spinning), so a pass that failed is retried. The report of
    /// each pass is dropped: [`Tiered::pending`] and [`Tiered::clashed`] say what is left.
    ///
    /// The task stops when the returned handle is dropped, possibly in the middle of a
    /// pass. A copy that reached the cold tier but was not recorded is found there by the
    /// next pass. A removal from the hot tier that was not recorded leaves the object
    /// counted in [`Tiered::hot_bytes`] until a store is opened again over the backends.
    ///
    /// Must be called inside a tokio runtime.
    #[must_use]
    pub fn spawn_background(self: &Arc<Self>, retry_every: Duration) -> Background {
        let tiered = Arc::clone(self);
        let every = retry_every.max(MIN_RETRY);
        let task: JoinHandle<()> = tokio::spawn(async move {
            loop {
                tiered.maintain().await;
                let _ = tokio::time::timeout(every, tiered.wake.notified()).await;
            }
        });
        Background { task }
    }
}

/// Records a failed step and says whether the pass is to stop: the failure says a backend is
/// unavailable, so the next object would fail the same way.
fn note_failure(failed: &mut Vec<(String, Error)>, key: String, error: Error) -> bool {
    let stop = error.is_unavailable();
    failed.push((key, error));
    stop
}

/// The handle of the task [`Tiered::spawn_background`] started. Dropping it stops the task.
pub struct Background {
    task: JoinHandle<()>,
}

impl Drop for Background {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[async_trait]
impl Backend for Tiered {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let size = body.len() as u64;
        if let Err(error) = self.hot.put(key, body).await {
            if matches!(error, Error::AlreadyExists(_)) {
                self.adopt(key).await;
            }
            return Err(error);
        }
        self.state().insert(key, size, false);
        self.wake.notify_one();
        Ok(())
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        match self.hot.get(key, range).await {
            Ok(body) => {
                self.state().touch(key);
                Ok(body)
            }
            Err(Error::NotFound(_)) => self.cold.get(key, range).await,
            Err(error) => Err(error),
        }
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        match self.hot.head(key).await {
            Err(Error::NotFound(_)) => self.cold.head(key).await,
            found => found,
        }
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        let _turn = self.turn.lock().await;
        self.hot.delete(key).await?;
        self.state().remove(key);
        self.cold.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        let mut merged: BTreeMap<String, u64> = BTreeMap::new();
        for object in self.hot.list(prefix).await? {
            merged.insert(object.key, object.size);
        }
        for object in self.cold.list(prefix).await? {
            merged.entry(object.key).or_insert(object.size);
        }
        let objects: Vec<Listed> = merged
            .into_iter()
            .map(|(key, size)| Listed { key, size })
            .collect();
        Ok(objects)
    }
}
