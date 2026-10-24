//! A wrapper that fails or delays the calls of another [`Backend`], for tests.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;

use crate::{Backend, ByteRange, Error, Listed, Meta};

/// The kinds of call a [`FaultyBackend`] tells apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    /// [`Backend::put`].
    Put,
    /// [`Backend::get`].
    Get,
    /// [`Backend::head`].
    Head,
    /// [`Backend::delete`].
    Delete,
    /// [`Backend::list`].
    List,
}

#[derive(Default)]
struct Plan {
    /// How many more calls of a kind fail.
    fail: BTreeMap<Op, u32>,
    /// How long a call of a kind waits before it runs.
    delay: BTreeMap<Op, Duration>,
    /// Whether every call fails.
    down: bool,
    /// How many calls of a kind were made, failed or not.
    calls: BTreeMap<Op, u64>,
}

/// Passes every call to the backend it wraps, unless it was told to fail or delay it.
///
/// A failed call returns [`Error::Unavailable`] without reaching the wrapped backend.
pub struct FaultyBackend {
    inner: Arc<dyn Backend>,
    plan: Mutex<Plan>,
}

impl FaultyBackend {
    /// A wrapper that passes everything through.
    #[must_use]
    pub fn new(inner: Arc<dyn Backend>) -> Self {
        Self {
            inner,
            plan: Mutex::new(Plan::default()),
        }
    }

    /// Fails the next `count` calls of kind `op`.
    pub fn fail_next(&self, op: Op, count: u32) {
        self.plan().fail.insert(op, count);
    }

    /// Fails every call while `down`, whatever its kind.
    pub fn set_down(&self, down: bool) {
        self.plan().down = down;
    }

    /// Makes every call of kind `op` wait `delay` (tokio time) before it runs, or stops
    /// doing so for `None`.
    pub fn set_delay(&self, op: Op, delay: Option<Duration>) {
        let mut plan = self.plan();
        match delay {
            Some(delay) => plan.delay.insert(op, delay),
            None => plan.delay.remove(&op),
        };
    }

    /// How many calls of kind `op` were made, the failed ones included.
    #[must_use]
    pub fn calls(&self, op: Op) -> u64 {
        self.plan().calls.get(&op).copied().unwrap_or(0)
    }

    fn plan(&self) -> MutexGuard<'_, Plan> {
        // No method panics while holding the lock, so a poisoned plan is still whole.
        self.plan.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts the call, and says how long it is to wait and whether it is to fail.
    fn arrive(&self, op: Op) -> (Option<Duration>, bool) {
        let mut plan = self.plan();
        *plan.calls.entry(op).or_insert(0) += 1;
        let delay = plan.delay.get(&op).copied();
        let down = plan.down;
        let left = plan.fail.entry(op).or_insert(0);
        let injected = *left > 0;
        *left = left.saturating_sub(1);
        (delay, down | injected)
    }

    async fn enter(&self, op: Op) -> Result<(), Error> {
        let (delay, fails) = self.arrive(op);
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        if fails {
            return Err(Error::Unavailable(format!("{op:?} failed on purpose")));
        }
        Ok(())
    }
}

#[async_trait]
impl Backend for FaultyBackend {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.enter(Op::Put).await?;
        self.inner.put(key, body).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.enter(Op::Get).await?;
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.enter(Op::Head).await?;
        self.inner.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.enter(Op::Delete).await?;
        self.inner.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.enter(Op::List).await?;
        self.inner.list(prefix).await
    }
}
