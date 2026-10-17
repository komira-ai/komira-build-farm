//! A state store that can be made to interleave, conflict or fail, over the fake with its
//! injectable clock: the membership tests' way of deciding what another frontend does at
//! the moment between this one's reads and its commit.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Op, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};

type Hook = Box<dyn FnOnce() + Send>;

pub struct Hooked {
    pub fake: Arc<Fake>,
    pub clock: Arc<ManualClock>,
    after_get: Mutex<Option<(String, Hook)>>,
    conflict_on: Mutex<Option<String>>,
    /// Per batch that wrote the `map` key: whether it conflicted.
    pub map_writes: Mutex<Vec<bool>>,
    /// How many of the next `get`s fail with a store error.
    pub fail_gets: AtomicUsize,
    /// How many of the next `reap_expired` calls fail with a store error.
    pub fail_reaps: AtomicUsize,
    /// How many of the next `get`s panic (a store that crashes inside its blocking task).
    pub panic_gets: AtomicUsize,
}

impl Hooked {
    pub fn new() -> Arc<Hooked> {
        let clock = Arc::new(ManualClock::default());
        Arc::new(Hooked {
            fake: Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None)),
            clock,
            after_get: Mutex::new(None),
            conflict_on: Mutex::new(None),
            map_writes: Mutex::new(Vec::new()),
            fail_gets: AtomicUsize::new(0),
            panic_gets: AtomicUsize::new(0),
            fail_reaps: AtomicUsize::new(0),
        })
    }

    /// Runs `hook` once, right after the next `get` or scan of a key that starts with `prefix`
    /// has read its answer: the answer the caller keeps is already stale when it runs.
    pub fn after_get(&self, prefix: &str, hook: impl FnOnce() + Send + 'static) {
        *self
            .after_get
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((prefix.to_owned(), Box::new(hook)));
    }

    /// Makes every transaction that touches a key (or counter) starting with `prefix`
    /// conflict, until `None`.
    pub fn conflict_on(&self, prefix: Option<&str>) {
        *self
            .conflict_on
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = prefix.map(str::to_owned);
    }
}

impl Hooked {
    /// Runs the hook if a read of `key` is the one it waits for.
    fn fire(&self, key: &str) {
        let hook = {
            let mut slot = self
                .after_get
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            match slot.take() {
                Some((prefix, hook)) if key.starts_with(&prefix) => Some(hook),
                other => {
                    *slot = other;
                    None
                }
            }
        };
        if let Some(hook) = hook {
            hook();
        }
    }
}

fn touches(txn: &Txn, prefix: &str) -> bool {
    txn.ops().iter().any(|op| match op {
        Op::Put { key, .. } | Op::Delete { key, .. } | Op::Check { key, .. } => {
            key.starts_with(prefix)
        }
        Op::Next { counter } => counter.starts_with(prefix),
    })
}

fn writes_map(txn: &Txn) -> bool {
    txn.ops()
        .iter()
        .any(|op| matches!(op, Op::Put { key, .. } if key == "map"))
}

impl Store for Hooked {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        let prefix = self
            .conflict_on
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let refused = |txn: &Txn| prefix.as_deref().is_some_and(|p| touches(txn, p));
        let mut answers = Vec::new();
        for txn in batch {
            let answer = if refused(&txn) {
                Err(Conflict {
                    index: 0,
                    key: String::new(),
                })
            } else {
                self.fake.txn(txn.clone())?
            };
            if writes_map(&txn) {
                self.map_writes
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(answer.is_err());
            }
            answers.push(answer);
        }
        Ok(answers)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        if self
            .fail_gets
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(StoreError::Failed("injected".to_owned()));
        }
        assert!(
            self.panic_gets
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_err(),
            "injected panic"
        );
        let answer = self.fake.get(key)?;
        self.fire(key);
        Ok(answer)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        let answer = self.fake.scan_prefix(prefix)?;
        self.fire(prefix);
        Ok(answer)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        let answer = self.fake.scan_range(scan)?;
        self.fire(scan.prefix);
        Ok(answer)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.fake.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        if self
            .fail_reaps
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(StoreError::Failed("injected".to_owned()));
        }
        self.fake.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.fake.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.fake.now()
    }
}
