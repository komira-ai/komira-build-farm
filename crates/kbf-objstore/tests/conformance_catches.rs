//! The conformance suite must fail a broken store, and name the right check.
//!
//! Each store here is the in-memory fake with one behaviour broken. Each test checks that
//! the suite reports that one check as failed and every other check as passed, so a
//! suite that stops checking (or that fails everything) goes red.

use std::time::SystemTime;

use bytes::Bytes;
use futures::executor::block_on;
use kbf_objstore::conformance::{self, Check, Outcome, Report};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};

const ALL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: true,
};

#[derive(Clone, Copy)]
enum Defect {
    /// Returns the whole object for any range.
    IgnoresRange,
    /// Returns one byte short of what was asked for.
    ShortRead,
    /// Claims Object Lock but stores the object unlocked, so delete removes it.
    DeletesLocked,
    /// Ends the listing one page early.
    DropsLastPage,
    /// Returns every key on one page, whatever page size was asked.
    IgnoresPageSize,
    /// Claims conditional writes but overwrites.
    Overwrites,
    /// Answers a missing key with an empty object.
    MissingIsEmpty,
    /// Claims no Object Lock and silently stores a retained object unlocked.
    DropsRetention,
}

struct Broken {
    inner: MemoryStore,
    claims: Capabilities,
    defect: Defect,
}

impl Broken {
    fn new(defect: Defect) -> Self {
        let (inner, claims) = match defect {
            Defect::DeletesLocked => (
                Capabilities {
                    object_lock: false,
                    ..ALL
                },
                ALL,
            ),
            Defect::Overwrites => (
                Capabilities {
                    conditional_put: false,
                    ..ALL
                },
                ALL,
            ),
            Defect::DropsRetention => (Capabilities::default(), Capabilities::default()),
            _ => (ALL, ALL),
        };
        Self {
            inner: MemoryStore::new(inner),
            claims,
            defect,
        }
    }
}

impl ObjectStore for Broken {
    fn capabilities(&self) -> Capabilities {
        self.claims
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        let retain_until = match self.defect {
            Defect::DeletesLocked | Defect::DropsRetention => None,
            _ => retain_until,
        };
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        match self.defect {
            Defect::IgnoresRange => {
                let all = ByteRange::new(0, u64::MAX / 2).unwrap();
                self.inner.get_range(key, all).await
            }
            Defect::ShortRead => {
                let b = self.inner.get_range(key, range).await?;
                Ok(b.slice(..b.len().saturating_sub(1)))
            }
            Defect::MissingIsEmpty => match self.inner.get_range(key, range).await {
                Err(ObjectStoreError::NotFound(_)) => Ok(Bytes::new()),
                other => other,
            },
            _ => self.inner.get_range(key, range).await,
        }
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        match self.defect {
            Defect::IgnoresPageSize => self.inner.list(prefix, after, PageSize::MAX).await,
            Defect::DropsLastPage => {
                let mut page = self.inner.list(prefix, after, max_keys).await?;
                if let Some(next) = &page.next {
                    let following = self.inner.list(prefix, Some(next), max_keys).await?;
                    if following.next.is_none() {
                        page.next = None;
                    }
                }
                Ok(page)
            }
            _ => self.inner.list(prefix, after, max_keys).await,
        }
    }
}

fn run(defect: Defect) -> Report {
    let store = Broken::new(defect);
    let prefix = KeyPrefix::new("conformance/").unwrap();
    block_on(conformance::run(&store, &prefix))
}

/// Asserts that exactly the checks in `failed` failed.
fn assert_fails_only(report: &Report, failed: &[Check]) {
    let got: Vec<Check> = report.failures().map(|(c, _)| c).collect();
    assert_eq!(got, failed, "\n{report}");
}

/// Catches: a suite that compares only lengths or only the first bytes, so a store
/// answering every ranged read with the whole object passes (the 4 MiB read windows
/// would then each pull a 128 MiB segment).
#[test]
fn a_store_that_ignores_range_fails() {
    let report = run(Defect::IgnoresRange);
    assert_fails_only(&report, &[Check::RangeBoundaries]);
}

/// Catches: a suite that does not check the length of what a range returns.
#[test]
fn a_store_that_reads_short_fails() {
    let report = run(Defect::ShortRead);
    assert_fails_only(
        &report,
        &[
            Check::RoundTrip,
            Check::RangeBoundaries,
            Check::PutNewRefusesOverwrite,
            Check::ObjectLock,
        ],
    );
}

/// Catches: a suite that trusts a store's Object Lock claim without deleting a retained
/// object, so `audit` could land on a store that loses it.
#[test]
fn a_store_that_claims_object_lock_but_deletes_a_locked_object_fails() {
    let report = run(Defect::DeletesLocked);
    assert_fails_only(&report, &[Check::ObjectLock]);
    let Some(Outcome::Failed(why)) = report.outcome(Check::ObjectLock) else {
        unreachable!("checked above");
    };
    assert!(why.contains("the object no longer reads"), "{why}");
}

/// Catches: a suite that stops at the first page without a token or never checks that
/// every key came back, so rebuild and sweep would miss the tail of a bucket.
#[test]
fn a_store_whose_listing_drops_the_last_page_fails() {
    let report = run(Defect::DropsLastPage);
    assert_fails_only(&report, &[Check::ListPagination]);
}

/// Catches: a suite whose listing check passes without ever paging.
#[test]
fn a_store_that_ignores_page_size_fails() {
    let report = run(Defect::IgnoresPageSize);
    assert_fails_only(&report, &[Check::ListPagination]);
}

/// Catches: a suite that trusts a conditional-write claim without trying an overwrite.
#[test]
fn a_store_that_claims_conditional_writes_but_overwrites_fails() {
    let report = run(Defect::Overwrites);
    assert_fails_only(&report, &[Check::PutNewRefusesOverwrite]);
}

/// Catches: a suite that accepts any answer for a missing key, or that does not read
/// after a delete; a store answering absent keys with empty bytes would make kbf serve
/// an empty blob for a lost one.
#[test]
fn a_store_that_answers_missing_keys_with_empty_bytes_fails() {
    let report = run(Defect::MissingIsEmpty);
    assert_fails_only(&report, &[Check::MissingObject, Check::DeleteIdempotent]);
}

/// Catches: a store without Object Lock that accepts a retention date and stores the
/// object unlocked; kbf requires a refusal, never a silent unlock.
#[test]
fn a_store_that_drops_retention_silently_fails() {
    let report = run(Defect::DropsRetention);
    assert_fails_only(&report, &[Check::ObjectLock]);
}
