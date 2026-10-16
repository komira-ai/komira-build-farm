//! The conformance suite must fail a broken store, and name the right check.
//!
//! Each store here is the in-memory fake with one behaviour broken. Each test checks that
//! the suite reports that one check as failed and every other check as passed, so a
//! suite that stops checking (or that fails everything) goes red.

use std::time::SystemTime;

use bytes::Bytes;
use futures::StreamExt;
use futures::executor::block_on;
use kbf_objstore::conformance::{self, Check, Outcome, Report};
use kbf_objstore::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey,
    ObjectStore, ObjectStoreError, PageSize,
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
    /// Ends the listing after a full page of the largest size, as a client that reads a
    /// full 1000-key page as the last would.
    EndsAtAFullLargestPage,
    /// Starts a listing after a key that exists at that key, not after it.
    StartsAtTheAfterKey,
    /// Claims conditional writes but overwrites.
    Overwrites,
    /// Answers a missing key with an empty object.
    MissingIsEmpty,
    /// Claims no Object Lock and silently stores a retained object unlocked.
    DropsRetention,
    /// Writes a streamed body's bytes as they came even when it failed or ended short.
    KeepsBrokenStream,
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

    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        mut body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        if !matches!(self.defect, Defect::KeepsBrokenStream) {
            let bytes = kbf_objstore::gather(body, len).await?;
            return self.put_new(key, bytes, None).await;
        }
        let mut got = Vec::new();
        while let Some(Ok(piece)) = body.next().await {
            got.extend_from_slice(&piece);
        }
        self.inner.put_new(key, Bytes::from(got), None).await
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
            Defect::StartsAtTheAfterKey => {
                let all = self.inner.list(prefix, None, PageSize::MAX).await?;
                let at = after.and_then(|t| {
                    all.objects
                        .iter()
                        .position(|o| ListToken::after(&o.key) == *t)
                });
                let Some(at) = at else {
                    return self.inner.list(prefix, after, max_keys).await;
                };
                let start = (at > 0).then(|| ListToken::after(&all.objects[at - 1].key));
                self.inner.list(prefix, start.as_ref(), max_keys).await
            }
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
            Defect::EndsAtAFullLargestPage => {
                let mut page = self.inner.list(prefix, after, max_keys).await?;
                if max_keys == PageSize::MAX && page.objects.len() == usize::from(max_keys.get()) {
                    page.next = None;
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
/// would then each pull a 128 MiB segment, and a rebuild would read every segment
/// whole to find its footer).
#[test]
fn a_store_that_ignores_range_fails() {
    let report = run(Defect::IgnoresRange);
    assert_fails_only(
        &report,
        &[
            Check::RangeBoundaries,
            Check::FooterReads,
            Check::StreamedPut,
        ],
    );
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
            Check::FooterReads,
            Check::PutNewRefusesOverwrite,
            Check::ObjectLock,
            Check::StreamedPut,
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
    assert_fails_only(
        &report,
        &[
            Check::ListPagination,
            Check::ListStartAfter,
            Check::ListPastPageCap,
            Check::FooterReads,
        ],
    );
}

/// Catches: a suite whose listings never fill a page of the largest size, so a store
/// (or an S3 client) that ends a listing at a full 1000-key page passes, and a rebuild
/// over a bucket of more than 1000 segments would stop at the first 1000.
#[test]
fn a_store_whose_listing_ends_at_a_full_largest_page_fails() {
    let report = run(Defect::EndsAtAFullLargestPage);
    assert_fails_only(&report, &[Check::ListPastPageCap]);
    let Some(Outcome::Failed(why)) = report.outcome(Check::ListPastPageCap) else {
        panic!("{report}")
    };
    assert!(why.contains("got 1000 keys in 1 pages"), "{why}");
}

/// Catches: a suite that never starts a listing after a key, or does not check where
/// it starts, so a rebuild resumed from its last key would read that key twice (and a
/// store that does this repeats a key between every two pages).
#[test]
fn a_store_whose_listing_starts_at_the_after_key_fails() {
    let report = run(Defect::StartsAtTheAfterKey);
    assert_fails_only(
        &report,
        &[
            Check::ListPagination,
            Check::ListStartAfter,
            Check::ListPastPageCap,
            Check::ListEmptyPrefix,
        ],
    );
}

/// Catches: a suite whose listing check passes without ever paging.
#[test]
fn a_store_that_ignores_page_size_fails() {
    let report = run(Defect::IgnoresPageSize);
    assert_fails_only(
        &report,
        &[
            Check::ListPagination,
            Check::ListStartAfter,
            Check::ListEmptyPrefix,
        ],
    );
}

/// Catches: a suite that trusts a conditional-write claim without trying an overwrite.
#[test]
fn a_store_that_claims_conditional_writes_but_overwrites_fails() {
    let report = run(Defect::Overwrites);
    assert_fails_only(
        &report,
        &[Check::PutNewRefusesOverwrite, Check::StreamedPut],
    );
}

/// Catches: a suite that accepts any answer for a missing key, or that does not read
/// after a delete; a store answering absent keys with empty bytes would make kbf serve
/// an empty blob for a lost one.
#[test]
fn a_store_that_answers_missing_keys_with_empty_bytes_fails() {
    let report = run(Defect::MissingIsEmpty);
    assert_fails_only(
        &report,
        &[
            Check::MissingObject,
            Check::DeleteIdempotent,
            Check::StreamedPut,
        ],
    );
}

/// Catches: a store without Object Lock that accepts a retention date and stores the
/// object unlocked; kbf requires a refusal, never a silent unlock.
#[test]
fn a_store_that_drops_retention_silently_fails() {
    let report = run(Defect::DropsRetention);
    assert_fails_only(&report, &[Check::ObjectLock]);
}

/// Catches: a suite that trusts a streamed write without feeding it a body that fails
/// or ends short; a store that keeps what it got would hold a truncated object under a
/// key that names a whole one (a segment whose footer is missing).
#[test]
fn a_store_that_keeps_a_broken_streamed_body_fails() {
    let report = run(Defect::KeepsBrokenStream);
    assert_fails_only(&report, &[Check::StreamedPut]);
    let Some(Outcome::Failed(why)) = report.outcome(Check::StreamedPut) else {
        unreachable!("checked above");
    };
    assert!(why.contains("a body that failed was written"), "{why}");
}
