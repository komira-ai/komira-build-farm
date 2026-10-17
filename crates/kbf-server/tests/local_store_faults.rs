//! A store that fails, through [`LocalStore::over`]: the error a daemon in the server's
//! process sees is the store's own, with the gRPC code it answers a remote daemon, and
//! never "absent".
//!
//! [`Faulty`] is a [`Storage`] whose every call fails with the error it was made with.
//! Its `Segments` service is the one a real store over an empty cache gives, as the
//! trait asks for a service and not a result.

use std::sync::Arc;

use bytes::Bytes;
use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_meta::{ActionRecord, Closure, Role};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::segments_server::Segments;
use kbf_segments::sha256;
use kbf_server::LocalStore;
use kbf_storage::{Answer, PutStream, ReadStream, Storage, StorageError};
use kbf_types::Digest;
use tonic::{Code, Status};

/// The error a store's backend gave.
#[derive(Debug, thiserror::Error)]
#[error("the disk is gone")]
struct Backend;

struct Faulty {
    error: StorageError,
    segments: CacheStorage<kbf_front::MemoryMetaLog, kbf_objstore::MemoryStore>,
}

impl Faulty {
    fn new(error: StorageError) -> Self {
        Self {
            error,
            segments: CacheStorage::new(Arc::new(Cache::memory()), SegmentsConfig::DEFAULT),
        }
    }

    fn fail<'a, T: Send + 'a>(&self) -> Answer<'a, T> {
        let error = self.error.clone();
        Box::pin(async move { Err(error) })
    }
}

impl Storage for Faulty {
    fn find_missing<'a>(&'a self, _digests: &'a [Digest]) -> Answer<'a, Vec<Digest>> {
        self.fail()
    }

    fn read_blob<'a>(&'a self, _digest: &'a Digest) -> Answer<'a, Bytes> {
        self.fail()
    }

    fn read_range<'a>(
        &'a self,
        _digest: &'a Digest,
        _offset: u64,
        _limit: u64,
    ) -> Answer<'a, ReadStream> {
        self.fail()
    }

    fn put(&self, _bytes: Bytes) -> Answer<'_, Digest> {
        self.fail()
    }

    fn stream(&self, _size: u64) -> Option<Box<dyn PutStream>> {
        None
    }

    fn put_streamed(
        &self,
        _size: u64,
        _bytes: futures::stream::BoxStream<'static, std::io::Result<Bytes>>,
        _claim: futures::future::BoxFuture<'static, Option<Digest>>,
    ) -> Answer<'_, Option<Digest>> {
        self.fail()
    }

    fn segments(&self) -> Arc<dyn Segments> {
        self.segments.segments()
    }

    fn action_result<'a>(
        &'a self,
        _action: &'a Digest,
    ) -> Answer<'a, Option<(Digest, ActionResult)>> {
        self.fail()
    }

    fn prepare_action_result<'a>(&'a self, _result: &'a ActionResult) -> Answer<'a, ActionRecord> {
        self.fail()
    }

    fn commit_action_record(
        &self,
        _role: Role,
        _action: Digest,
        _record: ActionRecord,
    ) -> Answer<'_, ()> {
        self.fail()
    }
}

/// Every call of a store that fails with `error` answers `error`, and the daemon's
/// calls answer gRPC code `code`.
async fn every_call_answers(error: StorageError, code: Code) {
    let message = error.to_string();
    let digest = sha256(b"a blob the store does hold, if it could answer");
    let faulty = Arc::new(Faulty::new(error));

    // The calls a daemon in the process makes, through `LocalStore`.
    let store = LocalStore::over(faulty.clone());
    let answers = [
        store
            .find_missing(&[digest])
            .await
            .expect_err("find_missing"),
        store.read_blob(&digest).await.expect_err("read_blob"),
        store.put(b"bytes".to_vec()).await.expect_err("put"),
    ];
    for answer in answers {
        assert_eq!(answer.to_string(), message);
        assert_eq!(Status::from(answer).code(), code, "{message}");
    }
    assert_eq!(Arc::strong_count(&store.segments()), 1, "its own service");

    // The rest of the trait, which `LocalStore` does not carry, fails the same way.
    let record = ActionRecord {
        result: digest,
        closure: Closure::new(),
    };
    let rest = [
        faulty.read_range(&digest, 0, 0).await.err(),
        faulty.action_result(&digest).await.err(),
        faulty
            .prepare_action_result(&ActionResult::default())
            .await
            .err(),
        faulty
            .commit_action_record(Role::Daemon, digest, record)
            .await
            .err(),
    ];
    for answer in rest {
        assert_eq!(answer.expect("fails").to_string(), message);
    }
    assert!(faulty.stream(1).is_none());
}

/// Catches a store that could not answer taken as an absent blob: FindMissing naming a
/// held blob as missing (so it is uploaded again over a failing store), a read of it
/// answering NOT_FOUND (a cache miss, so the action runs again), a write that reports
/// success. A store that is down must be UNAVAILABLE to the daemon, which retries.
#[tokio::test]
async fn an_unavailable_store_is_unavailable_never_absent() {
    every_call_answers(
        StorageError::Unavailable(Arc::new(Backend)),
        Code::Unavailable,
    )
    .await;
}

/// Catches a held blob whose object cannot be read answered as an absent one: NOT_FOUND
/// sends the daemon to re-upload what the store still lists as present.
#[tokio::test]
async fn an_unreadable_blob_is_unavailable_never_absent() {
    let digest = sha256(b"a blob the store does hold, if it could answer");
    every_call_answers(StorageError::Unreachable(digest), Code::Unavailable).await;
}

/// Catches `LocalStore` rewriting errors it should pass on: a blob the store really does
/// not hold is still NOT_FOUND, so the other two tests are not satisfied by an adapter
/// that answers UNAVAILABLE for everything.
#[tokio::test]
async fn an_absent_blob_is_still_not_found() {
    let digest = sha256(b"never stored");
    let store = LocalStore::over(Arc::new(Faulty::new(StorageError::NotFound(digest))));
    let error = store.read_blob(&digest).await.expect_err("absent");
    assert!(matches!(error, StorageError::NotFound(d) if d == digest));
    assert_eq!(Status::from(error).code(), Code::NotFound);
}
