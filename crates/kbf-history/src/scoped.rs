//! An object store that refuses every key outside one prefix.
//!
//! The history writer holds its store through [`Scoped`], so a bug that builds a `cas/`
//! key (or any key outside `history/`) is refused before it reaches the bucket. This is
//! defence in depth: the boundary is the credential the frontend is given, which the
//! object store enforces; a frontend holding a full credential would still pass this.

use std::future::Future;
use std::time::SystemTime;

use bytes::Bytes;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

/// `inner`, restricted to keys under `prefix`.
#[derive(Debug)]
pub struct Scoped<S> {
    inner: S,
    prefix: KeyPrefix,
}

impl<S> Scoped<S> {
    /// `inner` restricted to the history root's prefix (`<root>history/`).
    #[must_use]
    pub fn history(inner: S, root: &crate::keys::HistoryRoot) -> Scoped<S> {
        Scoped {
            inner,
            prefix: root.prefix().clone(),
        }
    }

    /// The prefix every key must start with.
    #[must_use]
    pub fn prefix(&self) -> &KeyPrefix {
        &self.prefix
    }

    /// The wrapped store.
    #[must_use]
    pub fn inner(&self) -> &S {
        &self.inner
    }
}

/// The answer a store gives a credential that may not touch a key, so a caller treats
/// a refused key as it would an access-denied bucket.
fn refused(what: &str, prefix: &KeyPrefix) -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 403,
        code: "AccessDenied".to_owned(),
        message: format!("{what} is outside {}", prefix.as_str()),
    }
}

/// Whether `key` is under `prefix`, or the error that refuses it.
fn check(prefix: &KeyPrefix, key: &ObjectKey) -> Result<(), ObjectStoreError> {
    if prefix.matches(key) {
        Ok(())
    } else {
        Err(refused(key.as_str(), prefix))
    }
}

/// Whether a listing of `asked` stays under `prefix`, or the error that refuses it.
fn check_prefix(prefix: &KeyPrefix, asked: &KeyPrefix) -> Result<(), ObjectStoreError> {
    if asked.as_str().starts_with(prefix.as_str()) {
        Ok(())
    } else {
        Err(refused(asked.as_str(), prefix))
    }
}

impl<S: ObjectStore> ObjectStore for Scoped<S> {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> impl Future<Output = Result<(), ObjectStoreError>> + Send {
        let checked = check(&self.prefix, key);
        async move {
            checked?;
            self.inner.put_new(key, body, retain_until).await
        }
    }

    fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> impl Future<Output = Result<Bytes, ObjectStoreError>> + Send {
        let checked = check(&self.prefix, key);
        async move {
            checked?;
            self.inner.get_range(key, range).await
        }
    }

    fn may_hold(
        &self,
        key: &ObjectKey,
    ) -> impl Future<Output = Result<bool, ObjectStoreError>> + Send {
        let checked = check(&self.prefix, key);
        async move {
            checked?;
            self.inner.may_hold(key).await
        }
    }

    fn delete(&self, key: &ObjectKey) -> impl Future<Output = Result<(), ObjectStoreError>> + Send {
        let checked = check(&self.prefix, key);
        async move {
            checked?;
            self.inner.delete(key).await
        }
    }

    fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> impl Future<Output = Result<ListPage, ObjectStoreError>> + Send {
        let checked = check_prefix(&self.prefix, prefix);
        async move {
            checked?;
            self.inner.list(prefix, after, max_keys).await
        }
    }
}
