//! What the suite leaves behind: nothing but what the store refuses to delete.
//!
//! The checks record every key they write in a [`Written`]; after the last check
//! [`sweep`] deletes exactly those keys, whether the checks passed or not. It never lists
//! to find what to delete, so no prefix (the empty one, or one that is the start of a
//! neighbour's) can make it delete a key the suite did not write.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::SystemTime;

use bytes::Bytes;
use futures::{StreamExt as _, stream};

use super::LOCK_KEY;
use crate::{BodyStream, KeyPrefix, ObjectKey, ObjectStore, ObjectStoreError};

/// How many deletes are in flight at once.
const DELETES: usize = 16;

/// What the sweep did to the keys the run wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cleanup {
    /// Objects deleted.
    pub deleted: usize,
    /// Objects the store refused to delete because they are still under Object Lock
    /// retention. Expected after a run against a store with Object Lock; not a failure.
    pub retained: Vec<ObjectKey>,
    /// Everything else that went wrong (a delete refused for another reason): the
    /// object may remain.
    pub failed: Vec<String>,
}

/// The keys a run has written, in the order they sort.
#[derive(Debug, Default)]
pub(super) struct Written(Mutex<BTreeSet<ObjectKey>>);

impl Written {
    /// `store.put_new`, remembering `key` for the sweep. The key is remembered whatever the
    /// answer (a put that errors may still have landed) except when the store says the
    /// key already existed: then it is not this run's to delete.
    pub(super) async fn put_new<S: ObjectStore>(
        &self,
        store: &S,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        let answer = store.put_new(key, body, retain_until).await;
        self.remember(key, &answer);
        answer
    }

    /// `store.put_stream`, remembered as [`Written::put_new`] does.
    pub(super) async fn put_stream<S: ObjectStore>(
        &self,
        store: &S,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        let answer = store.put_stream(key, len, body).await;
        self.remember(key, &answer);
        answer
    }

    fn remember(&self, key: &ObjectKey, answer: &Result<(), ObjectStoreError>) {
        if !matches!(answer, Err(ObjectStoreError::AlreadyExists(_))) {
            self.0
                .lock()
                .expect("no panic holds the lock")
                .insert(key.clone());
        }
    }
}

/// Deletes every key in `written`; reports what it could not.
pub(super) async fn sweep<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    written: &Written,
) -> Cleanup {
    let mut cleanup = Cleanup::default();
    let keys: Vec<ObjectKey> = written
        .0
        .lock()
        .expect("no panic holds the lock")
        .iter()
        .cloned()
        .collect();
    let lock_key = prefix.key(LOCK_KEY).ok();
    let answers: Vec<_> = stream::iter(&keys)
        .map(|k| async move { (k, store.delete(k).await) })
        .buffer_unordered(DELETES)
        .collect()
        .await;
    for (k, answer) in answers {
        match answer {
            Ok(()) => cleanup.deleted += 1,
            // S3 answers a delete of a retained object with a 403, not a name of its
            // own; the lock check's object is the only one the suite retains.
            Err(ObjectStoreError::Locked(_)) => cleanup.retained.push(k.clone()),
            Err(_) if lock_key.as_ref() == Some(k) => cleanup.retained.push(k.clone()),
            Err(e) => cleanup.failed.push(format!("delete {k}: {e}")),
        }
    }
    cleanup.retained.sort();
    cleanup.failed.sort();
    cleanup
}
