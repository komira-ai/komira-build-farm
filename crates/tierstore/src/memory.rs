//! An in-memory [`Backend`]: the executable spec of the contract, for tests.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::{Backend, ByteRange, Error, Listed, Meta};

/// A bucket held in memory. Its content tag is the hex of the SHA-256 of the body.
#[derive(Default)]
pub struct MemoryBackend {
    objects: Mutex<BTreeMap<String, Bytes>>,
}

impl MemoryBackend {
    /// An empty bucket.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn objects(&self) -> MutexGuard<'_, BTreeMap<String, Bytes>> {
        // No method panics while holding the lock, so a poisoned map is still whole.
        self.objects.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn object(&self, key: &str) -> Result<Bytes, Error> {
        let body = self.objects().get(key).cloned();
        body.ok_or_else(|| Error::NotFound(key.to_owned()))
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let mut objects = self.objects();
        if objects.contains_key(key) {
            return Err(Error::AlreadyExists(key.to_owned()));
        }
        objects.insert(key.to_owned(), body);
        Ok(())
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let body = self.object(key)?;
        match range {
            Some(range) => range.slice(&body),
            None => Ok(body),
        }
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let body = self.object(key)?;
        Ok(Meta {
            size: body.len() as u64,
            tag: hex::encode(Sha256::digest(&body)),
        })
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.objects().remove(key);
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        let objects = self.objects();
        let listed: Vec<Listed> = objects
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, body)| Listed {
                key: key.clone(),
                size: body.len() as u64,
            })
            .collect();
        Ok(listed)
    }
}
