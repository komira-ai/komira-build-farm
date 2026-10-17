//! The conformance suite must fail a broken store, and name the right check.
//!
//! Each store here is the in-memory fake with one behaviour broken. Each test checks that
//! the suite reports that one check as failed and every other check as passed, so a
//! suite that stops checking (or that fails everything) goes red.

use std::collections::HashMap;
use std::sync::Mutex;
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
    /// Answers a page size of 1 with pages of 2, as a client that sends `max-keys` only
    /// above 1 and a store whose default page is small would.
    IgnoresPageSizeOne,
    /// Answers a prefix that matches nothing with the keys that follow it.
    ListsPastAnEmptyPrefix,
    /// Lists only the keys with no `/` after the prefix, as a listing with a `/`
    /// delimiter would.
    GroupsAtSlash,
    /// Pages by position: its token is the count of keys listed so far, so a key written
    /// before the cursor repeats one and a key deleted before it skips one.
    PagesByPosition,
    /// Refuses every listing call that continues a listing, as a store whose
    /// continuation tokens expire at once would.
    RefusesSecondPages,
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
    /// [`Defect::PagesByPosition`]: how many keys were listed before each token issued.
    positions: Mutex<HashMap<ObjectKey, usize>>,
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
            positions: Mutex::default(),
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
            Defect::IgnoresPageSizeOne if max_keys.get() == 1 => {
                let two = PageSize::new(2).unwrap();
                self.inner.list(prefix, after, two).await
            }
            Defect::ListsPastAnEmptyPrefix => {
                let page = self.inner.list(prefix, after, max_keys).await?;
                let text = prefix.as_str().trim_end_matches('/');
                if !page.objects.is_empty() || after.is_some() || text.is_empty() {
                    return Ok(page);
                }
                let from = ListToken::after(&ObjectKey::new(text).unwrap());
                let everything = KeyPrefix::default();
                let past = self.inner.list(&everything, Some(&from), max_keys).await?;
                Ok(ListPage { next: None, ..past })
            }
            Defect::GroupsAtSlash => {
                let mut page = self.inner.list(prefix, after, max_keys).await?;
                page.objects
                    .retain(|o| !o.key.as_str()[prefix.as_str().len()..].contains('/'));
                Ok(page)
            }
            Defect::PagesByPosition => self.list_by_position(prefix, after, max_keys).await,
            Defect::RefusesSecondPages if after.is_some() => Err(ObjectStoreError::Service {
                status: 400,
                code: "InvalidArgument".into(),
                message: "The continuation token provided is incorrect".into(),
            }),
            _ => self.inner.list(prefix, after, max_keys).await,
        }
    }
}

impl Broken {
    /// A listing whose token stands for a position: the page after a token it issued
    /// starts that many keys into the prefix as it is now.
    async fn list_by_position(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        let mut all = Vec::new();
        let mut from = None;
        loop {
            let page = self
                .inner
                .list(prefix, from.as_ref(), PageSize::MAX)
                .await?;
            all.extend(page.objects);
            match page.next {
                Some(t) => from = Some(t),
                None => break,
            }
        }
        let issued = after.and_then(|t| {
            let positions = self.positions.lock().unwrap();
            positions
                .iter()
                .find(|(k, _)| prefix.matches(k) && ListToken::after(k) == *t)
                .map(|(_, n)| *n)
        });
        let skip = match (issued, after) {
            (Some(n), _) => n,
            (None, None) => 0,
            // A listing after a key: as the inner store, then counted as a position.
            (None, Some(t)) => {
                let page = self.inner.list(prefix, Some(t), max_keys).await?;
                page.objects.first().map_or(all.len(), |o| {
                    all.iter().take_while(|a| a.key < o.key).count()
                })
            }
        };
        let rest: Vec<_> = all.into_iter().skip(skip).collect();
        let more = rest.len() > usize::from(max_keys.get());
        let objects: Vec<_> = rest.into_iter().take(usize::from(max_keys.get())).collect();
        let next = more.then(|| ListToken::after(&objects.last().unwrap().key));
        Ok(self.issue(ListPage { objects, next }, skip))
    }

    /// Records the position after `page`, which starts `skip` keys into its prefix.
    fn issue(&self, page: ListPage, skip: usize) -> ListPage {
        if let Some(last) = page.next.as_ref().and(page.objects.last()) {
            let mut positions = self.positions.lock().unwrap();
            positions.insert(last.key.clone(), skip + page.objects.len());
        }
        page
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
            Check::ListPageSizes,
            Check::ListNested,
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
/// store that does this repeats a key between every two pages). The footer reads fail
/// too: their listing stream refuses a repeated key rather than read a segment twice.
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
            Check::ListPageSizes,
            Check::ListNested,
            Check::ListDuringWrites,
            Check::FooterReads,
        ],
    );
    let Some(Outcome::Failed(why)) = report.outcome(Check::FooterReads) else {
        panic!("{report}")
    };
    assert!(why.contains("keys must ascend"), "{why}");
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
            Check::ListPageSizes,
            Check::ListNested,
            Check::ListDuringWrites,
        ],
    );
}

/// Catches: a suite that never lists in pages of one key, so a client or store that
/// mishandles the smallest page passes.
#[test]
fn a_store_that_ignores_a_page_size_of_one_fails() {
    let report = run(Defect::IgnoresPageSizeOne);
    assert_fails_only(&report, &[Check::ListPageSizes, Check::ListNested]);
    let Some(Outcome::Failed(why)) = report.outcome(Check::ListPageSizes) else {
        panic!("{report}")
    };
    assert!(why.contains("page 1 has 2 keys; at most 1"), "{why}");
}

/// Catches: a suite that never lists a prefix holding no key, so a store that answers
/// one with what follows it passes, and a rebuild of an empty prefix would index
/// another prefix's objects.
#[test]
fn a_store_that_lists_past_an_empty_prefix_fails() {
    let report = run(Defect::ListsPastAnEmptyPrefix);
    assert_fails_only(&report, &[Check::ListNoMatch]);
}

/// Catches: a suite whose keys never nest under `/` below the listed prefix, so a store
/// (or client) that groups at `/` passes, and a rebuild over nested keys would see only
/// the top level.
#[test]
fn a_store_that_groups_keys_at_slash_fails() {
    let report = run(Defect::GroupsAtSlash);
    assert_fails_only(&report, &[Check::ListEmptyPrefix, Check::ListNested]);
    let Some(Outcome::Failed(why)) = report.outcome(Check::ListNested) else {
        panic!("{report}")
    };
    assert!(
        why.contains(
            "page by page Ok([(\"conformance/nested/A\", 1), (\"conformance/nested/a-b\", 1)"
        ),
        "{why}"
    );
}

/// Catches: a suite that never writes or deletes during a listing, so a store that pages
/// by position passes, and a rebuild racing new segments would read one twice or skip
/// one written before it began.
#[test]
fn a_store_that_pages_by_position_fails() {
    let report = run(Defect::PagesByPosition);
    assert_fails_only(&report, &[Check::ListDuringWrites]);
    let Some(Outcome::Failed(why)) = report.outcome(Check::ListDuringWrites) else {
        panic!("{report}")
    };
    assert!(why.contains("the keys present throughout came as"), "{why}");
}

/// Catches: a suite that takes a failed listing call for the end of a listing, or that
/// fails such a store without saying why: every check that lists past one page fails,
/// naming the store's answer.
#[test]
fn a_store_that_refuses_second_pages_fails() {
    let report = run(Defect::RefusesSecondPages);
    assert_fails_only(
        &report,
        &[
            Check::ListPagination,
            Check::ListStartAfter,
            Check::ListPastPageCap,
            Check::ListEmptyPrefix,
            Check::ListPageSizes,
            Check::ListNested,
            Check::ListDuringWrites,
            Check::FooterReads,
        ],
    );
    for (check, why) in report.failures() {
        assert!(why.contains("400 InvalidArgument"), "{check:?}: {why}");
    }
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
