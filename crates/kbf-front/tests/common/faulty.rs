//! An object store that fails on cue, and a metadata log that checks every `PutBlob`
//! against the bytes the store holds at the moment it commits.
//!
//! [`FaultyStore`] wraps a [`MemoryStore`]. Each `put_new` and each `get_range` takes
//! the next fault from its own queue; with the queue empty it passes the call through.
//! A fault can refuse the call, lose the answer of a call that succeeded, hold the call
//! until the test lets it go, or answer OK while storing bytes other than those sent.
//!
//! [`AuditLog`] wraps a [`MemoryMetaLog`]. When a `PutBlob` commits (or a `PutBlobs`,
//! for each of its entries), it reads the object the location names straight from the
//! inner store (past any fault) and records a violation unless the bytes there hash to
//! the blob's digest. So a cache that commits an index entry before its object is
//! durable, or at a location that does not hold the blob, leaves a violation behind
//! even if a later write fills the gap.

use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use bytes::Bytes;
use kbf_front::{MemoryMetaLog, MetaLog, MetaLogError};
use kbf_meta::{Applied, Command, Location, MetaState, ObjectId, Retention};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_types::Digest;
use tokio::sync::oneshot;

/// What the next `put_new` does instead of a plain write.
#[derive(Debug)]
pub enum PutFault {
    /// Answers a transport error; nothing is stored.
    Fail,
    /// Stores the object, then answers a transport error, as when the connection drops
    /// after the store committed. The caller cannot tell it from [`PutFault::Fail`].
    LoseAnswer,
    /// Answers OK but stores the body without its last `n` bytes.
    Truncate(usize),
    /// Answers OK and stores nothing.
    Drop,
    /// Answers OK but stores these bytes instead of the body.
    Replace(Bytes),
    /// Signals `entered` when the call arrives, waits for `release`, then stores the
    /// body and answers OK.
    Hold {
        /// Fired once the call has arrived (before anything is stored).
        entered: oneshot::Sender<()>,
        /// The call goes on once this fires or its sender is dropped.
        release: oneshot::Receiver<()>,
    },
}

/// What the next `get_range` answers instead of the stored bytes.
#[derive(Clone, Copy, Debug)]
pub enum GetFault {
    /// The request did not complete (a reset connection).
    Transport,
    /// The store answered 503 SlowDown.
    SlowDown,
    /// The store says no object has the key.
    NotFound,
    /// The store says the range starts past the end of the object.
    InvalidRange,
}

#[derive(Debug, Default)]
struct Script {
    puts: VecDeque<PutFault>,
    gets: VecDeque<GetFault>,
    put_keys: Vec<ObjectKey>,
}

struct Shared {
    inner: MemoryStore,
    script: Mutex<Script>,
}

/// A [`MemoryStore`] that fails on cue. Clones share one bucket and one script.
#[derive(Clone)]
pub struct FaultyStore(Arc<Shared>);

impl std::fmt::Debug for FaultyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // MemoryStore is not Debug; the script is what a failing test wants to see.
        f.debug_struct("FaultyStore")
            .field("script", &*self.script())
            .finish_non_exhaustive()
    }
}

impl FaultyStore {
    pub fn new(capabilities: Capabilities) -> Self {
        Self(Arc::new(Shared {
            inner: MemoryStore::new(capabilities),
            script: Mutex::new(Script::default()),
        }))
    }

    /// Queues `fault` for a coming `put_new`, after any already queued.
    pub fn on_put(&self, fault: PutFault) {
        self.script().puts.push_back(fault);
    }

    /// Queues `fault` for a coming `get_range`, after any already queued.
    pub fn on_get(&self, fault: GetFault) {
        self.script().gets.push_back(fault);
    }

    /// Every key `put_new` was called with, in order, faulted or not.
    pub fn put_keys(&self) -> Vec<ObjectKey> {
        self.script().put_keys.clone()
    }

    /// Whether every queued fault has been used.
    pub fn script_done(&self) -> bool {
        let s = self.script();
        s.puts.is_empty() && s.gets.is_empty()
    }

    /// The whole object under `key` as the inner bucket holds it, past any fault.
    pub async fn stored(&self, key: &ObjectKey) -> Option<Bytes> {
        // A range past the end reads what exists: the whole object.
        let whole = ByteRange::new(0, u64::from(u32::MAX)).expect("range");
        self.0.inner.get_range(key, whole).await.ok()
    }

    fn script(&self) -> MutexGuard<'_, Script> {
        self.0.script.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn transport() -> ObjectStoreError {
    ObjectStoreError::Transport(Box::new(io::Error::new(
        io::ErrorKind::ConnectionReset,
        "injected by FaultyStore",
    )))
}

impl ObjectStore for FaultyStore {
    fn capabilities(&self) -> Capabilities {
        self.0.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        let fault = {
            let mut s = self.script();
            s.put_keys.push(key.clone());
            s.puts.pop_front()
        };
        let inner = &self.0.inner;
        match fault {
            None => inner.put_new(key, body, retain_until).await,
            Some(PutFault::Fail) => Err(transport()),
            Some(PutFault::LoseAnswer) => {
                inner.put_new(key, body, retain_until).await?;
                Err(transport())
            }
            Some(PutFault::Truncate(n)) => {
                let keep = body.len().saturating_sub(n);
                inner.put_new(key, body.slice(..keep), retain_until).await
            }
            Some(PutFault::Drop) => Ok(()),
            Some(PutFault::Replace(other)) => inner.put_new(key, other, retain_until).await,
            Some(PutFault::Hold { entered, release }) => {
                let _ = entered.send(());
                let _ = release.await;
                inner.put_new(key, body, retain_until).await
            }
        }
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let fault = self.script().gets.pop_front();
        match fault {
            None => self.0.inner.get_range(key, range).await,
            Some(GetFault::Transport) => Err(transport()),
            Some(GetFault::SlowDown) => Err(ObjectStoreError::Service {
                status: 503,
                code: "SlowDown".to_owned(),
                message: "injected by FaultyStore".to_owned(),
            }),
            Some(GetFault::NotFound) => Err(ObjectStoreError::NotFound(key.clone())),
            Some(GetFault::InvalidRange) => Err(ObjectStoreError::InvalidRange {
                key: key.clone(),
                range,
            }),
        }
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.inner.list(prefix, after, max_keys).await
    }
}

/// A [`MemoryMetaLog`] that, as each `PutBlob` (or each entry of a `PutBlobs`) commits,
/// checks the store already holds the blob's bytes at the committed location.
#[derive(Debug)]
pub struct AuditLog {
    inner: MemoryMetaLog,
    store: FaultyStore,
    put_blobs: Mutex<Vec<Digest>>,
    violations: Mutex<Vec<String>>,
}

impl AuditLog {
    pub fn new(store: FaultyStore) -> Self {
        Self {
            inner: MemoryMetaLog::new(Retention::default()),
            store,
            put_blobs: Mutex::new(Vec::new()),
            violations: Mutex::new(Vec::new()),
        }
    }

    /// Every digest a `PutBlob` committed, in order.
    pub fn put_blobs(&self) -> Vec<Digest> {
        self.put_blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Every `PutBlob` that committed ahead of, or away from, its bytes.
    pub fn violations(&self) -> Vec<String> {
        self.violations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Checks `digest` is readable at `location` in the inner bucket right now.
    async fn audit(&self, digest: Digest, location: Location) {
        let Location { object, offset, .. } = location;
        let key = object_key(object);
        let problem = match self.store.stored(&key).await {
            None => Some(format!(
                "PutBlob {digest} committed before {key} was stored"
            )),
            Some(bytes) => {
                let start = usize::try_from(offset).expect("offset");
                let end = start.saturating_add(usize::try_from(digest.size_bytes).expect("size"));
                match bytes.get(start..end) {
                    Some(b) if kbf_segments::sha256(b) == digest => None,
                    _ => Some(format!(
                        "PutBlob {digest} committed at {key}+{offset}, which does not hold it"
                    )),
                }
            }
        };
        if let Some(p) = problem {
            self.violations
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(p);
        }
    }
}

/// The key the cache gives object `id` under the default prefix. It restates
/// `Cache::object_key` so the audit can run before a `Cache` exists; if the two ever
/// differ, every audit reports a violation.
fn object_key(id: ObjectId) -> ObjectKey {
    let (epoch, seq) = (id.epoch().get(), id.seq());
    KeyPrefix::default()
        .key(&format!("cas/{epoch:016x}/{seq:016x}"))
        .expect("key")
}

impl MetaLog for AuditLog {
    async fn commit(&self, command: Command) -> Result<Applied, MetaLogError> {
        let placed = match &command {
            Command::PutBlob { digest, location } => vec![(*digest, *location)],
            Command::PutBlobs(blobs) => blobs.clone(),
            _ => Vec::new(),
        };
        for (digest, location) in placed {
            self.put_blobs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(digest);
            self.audit(digest, location).await;
        }
        self.inner.commit(command).await
    }

    async fn query<R, F>(&self, f: F) -> Result<R, MetaLogError>
    where
        F: FnOnce(&MetaState) -> R + Send,
        R: Send,
    {
        self.inner.query(f).await
    }
}
