//! An object store that fails on cue, and a metadata log that checks every `PutBlob`
//! against the bytes the store holds at the moment it commits.
//!
//! [`FaultyStore`] wraps a [`MemoryStore`]. Each `put_new` and each `get_range` takes
//! the next fault from its own queue; with the queue empty it passes the call through.
//! A fault can refuse the call, lose the answer of a call that succeeded, hold the call
//! until the test lets it go, or answer OK while storing bytes other than those sent.
//!
//! Besides the queue, a store can fail every PUT of a segment that holds a given blob
//! ([`FaultyStore::fail_puts_holding`]), and hold every PUT at a gate
//! ([`FaultyStore::hold_puts`]) while it counts how many are in flight.
//!
//! [`AuditLog`] wraps a [`MemoryMetaLog`]. When a `PutBlob` commits (or a `PutBlobs`,
//! for each of its entries), it reads the object the location names straight from the
//! inner store (past any fault) and records a violation unless the bytes there hash to
//! the blob's digest. So a cache that commits an index entry before its object is
//! durable, or at a location that does not hold the blob, leaves a violation behind
//! even if a later write fills the gap.

use std::collections::{BTreeSet, VecDeque};
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
use kbf_segments::SegmentReader;
use kbf_types::Digest;
use tokio::sync::{oneshot, watch};

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
    /// Answers an S3 error with this HTTP status; nothing is stored.
    Status(u16),
    /// Panics, as a bug in the store client would.
    Panic,
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
    /// The length of the bytes each `get_range` answered with, in order.
    get_lens: Vec<usize>,
    poisoned: BTreeSet<Digest>,
    puts_in_flight: usize,
    puts_in_flight_peak: usize,
}

struct Shared {
    inner: MemoryStore,
    script: Mutex<Script>,
    /// Whether PUTs may go on (`true`) or wait at the gate.
    gate: watch::Sender<bool>,
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
            gate: watch::Sender::new(true),
        }))
    }

    /// Fails, with a transport error, every PUT of a segment whose footer names `digest`.
    pub fn fail_puts_holding(&self, digest: Digest) {
        self.script().poisoned.insert(digest);
    }

    /// Holds every PUT that arrives from now on (counted as in flight) until
    /// [`FaultyStore::release_puts`].
    pub fn hold_puts(&self) {
        self.0.gate.send_replace(false);
    }

    /// Lets every held PUT go on, and every later one through.
    pub fn release_puts(&self) {
        self.0.gate.send_replace(true);
    }

    /// The PUTs in flight now: arrived and not yet answered.
    pub fn puts_in_flight(&self) -> usize {
        self.script().puts_in_flight
    }

    /// The most PUTs that were in flight at once.
    pub fn puts_in_flight_peak(&self) -> usize {
        self.script().puts_in_flight_peak
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

    /// The length of the bytes each `get_range` answered with so far, in order: what a
    /// reader pulled from the store, and how much of it at a time.
    pub fn get_lens(&self) -> Vec<usize> {
        self.script().get_lens.clone()
    }

    /// Forgets the lengths recorded so far (a test's setup reads footers back).
    pub fn forget_gets(&self) {
        self.script().get_lens.clear();
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
    ObjectStoreError::Transport(Arc::new(io::Error::new(
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
        let poisoned = {
            let mut s = self.script();
            s.puts_in_flight += 1;
            s.puts_in_flight_peak = s.puts_in_flight_peak.max(s.puts_in_flight);
            holds_any(&body, &s.poisoned)
        };
        let _ = self.0.gate.subscribe().wait_for(|open| *open).await;
        let answer = self.put_now(key, body, retain_until, poisoned).await;
        self.script().puts_in_flight -= 1;
        answer
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let fault = self.script().gets.pop_front();
        match fault {
            None => {
                let bytes = self.0.inner.get_range(key, range).await;
                let len = bytes.as_ref().ok().map(Bytes::len);
                self.script().get_lens.extend(len);
                bytes
            }
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

/// Whether `body` parses as a segment whose footer names any of `digests`.
fn holds_any(body: &[u8], digests: &BTreeSet<Digest>) -> bool {
    SegmentReader::open(body).is_ok_and(|r| {
        r.footer()
            .entries()
            .iter()
            .any(|e| digests.contains(&e.digest))
    })
}

impl FaultyStore {
    /// One PUT past the gate: the poisoned failure, the next queued fault, or a plain
    /// write.
    async fn put_now(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
        poisoned: bool,
    ) -> Result<(), ObjectStoreError> {
        let fault = {
            let mut s = self.script();
            s.put_keys.push(key.clone());
            if poisoned {
                return Err(transport());
            }
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
            Some(PutFault::Status(status)) => Err(ObjectStoreError::Service {
                status,
                code: "Injected".to_owned(),
                message: "injected by FaultyStore".to_owned(),
            }),
            Some(PutFault::Panic) => panic!("injected by FaultyStore"),
            Some(PutFault::Replace(other)) => inner.put_new(key, other, retain_until).await,
            Some(PutFault::Hold { entered, release }) => {
                let _ = entered.send(());
                let _ = release.await;
                inner.put_new(key, body, retain_until).await
            }
        }
    }
}

/// A [`MemoryMetaLog`] that, as each `PutBlob` (or each entry of a `PutBlobs`) commits,
/// checks the store already holds the blob's bytes at the committed location.
#[derive(Debug)]
pub struct AuditLog {
    inner: MemoryMetaLog,
    store: FaultyStore,
    put_blobs: Mutex<Vec<Digest>>,
    batches: Mutex<Vec<usize>>,
    violations: Mutex<Vec<String>>,
    next_put_blobs: Mutex<Option<CommitFault>>,
}

/// What the next `PutBlobs` does instead of committing at once.
#[derive(Debug)]
pub enum CommitFault {
    /// Answers UNAVAILABLE; nothing is committed.
    Refuse,
    /// Signals `entered` when the command arrives, waits for `release`, then commits.
    Hold {
        /// Fired once the command has arrived (before it is applied).
        entered: oneshot::Sender<()>,
        /// The commit goes on once this fires or its sender is dropped.
        release: oneshot::Receiver<()>,
    },
}

impl AuditLog {
    pub fn new(store: FaultyStore) -> Self {
        Self {
            inner: MemoryMetaLog::new(Retention::default()),
            store,
            put_blobs: Mutex::new(Vec::new()),
            batches: Mutex::new(Vec::new()),
            violations: Mutex::new(Vec::new()),
            next_put_blobs: Mutex::new(None),
        }
    }

    /// Sets what the next `PutBlobs` does.
    pub fn on_put_blobs(&self, fault: CommitFault) {
        *self
            .next_put_blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(fault);
    }

    /// How many blobs each `PutBlobs` that reached the log named, in order.
    pub fn batches(&self) -> Vec<usize> {
        self.batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
        if let Command::PutBlobs(blobs) = &command {
            self.batches
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(blobs.len());
            let fault = self
                .next_put_blobs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            match fault {
                None => {}
                Some(CommitFault::Refuse) => {
                    return Err(MetaLogError::Unavailable("injected by AuditLog".to_owned()));
                }
                Some(CommitFault::Hold { entered, release }) => {
                    let _ = entered.send(());
                    let _ = release.await;
                }
            }
        }
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
