//! The listing checks that page through every page size, a prefix that matches nothing,
//! keys nested under `/`, and a listing that runs while keys are written and deleted.
//! Each listing is made twice, page by page and through [`listing::objects`] (the
//! stream callers use), and both must give the same, exact answer.

use futures::TryStreamExt as _;

use super::{CheckResult, Outcome, Written, child, key, pattern, put};
use crate::listing;
use crate::{KeyPrefix, ObjectStore, PageSize};

/// Keys the page-size check writes, and the page sizes it lists them in: one key a
/// page, sizes that divide the count exactly (the last page full), one that does not
/// (the last page short), the count itself, and more than the count.
const SIZED: usize = 6;
const SIZES: [u16; 6] = [1, 2, 3, 4, 6, 7];

/// The most pages any listing here may take: one key a page over the largest listing,
/// and some to spare.
const MOST_PAGES: usize = 16;

/// What a listing gave: each key with its size, in the order they came.
type Listed = Result<Vec<(String, u64)>, String>;

/// Lists `prefix` in pages of `page` keys, calling `between` with the page's number
/// after every page that has a next. Fails on a page larger than asked, or past
/// [`MOST_PAGES`] pages.
async fn pages<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    page: u16,
    mut between: impl AsyncFnMut(usize) -> Result<(), String>,
) -> Listed {
    let size = PageSize::new(page).expect("the suite's page sizes are in range");
    let (mut got, mut after) = (Vec::new(), None);
    for n in 1..=MOST_PAGES {
        let listed = store
            .list(prefix, after.as_ref(), size)
            .await
            .map_err(|e| format!("page {n}: {e}"))?;
        ensure!(
            listed.objects.len() <= usize::from(page),
            "page {n} has {} keys; at most {page} were asked for",
            listed.objects.len()
        );
        got.extend(
            listed
                .objects
                .into_iter()
                .map(|o| (o.key.as_str().to_owned(), o.size)),
        );
        let Some(next) = listed.next else {
            return Ok(got);
        };
        after = Some(next);
        between(n).await?;
    }
    Err(format!("did not end after {MOST_PAGES} pages"))
}

/// Lists `prefix` in pages of `page` keys through [`listing::objects`].
async fn streamed<S: ObjectStore>(store: &S, prefix: &KeyPrefix, page: u16) -> Listed {
    let size = PageSize::new(page).expect("the suite's page sizes are in range");
    listing::objects(store, prefix, None, size)
        .map_ok(|o| (o.key.as_str().to_owned(), o.size))
        .try_collect()
        .await
        .map_err(|e| e.to_string())
}

/// Lists `prefix` page by page and as a stream, both in pages of `page` keys, and
/// fails unless each gives exactly `want`.
async fn lists_exactly<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    page: u16,
    want: &[(String, u64)],
) -> Result<(), String> {
    let got = (
        pages(store, prefix, page, async |_| Ok(())).await,
        streamed(store, prefix, page).await,
    );
    let want = Ok(want.to_vec());
    ensure!(
        got == (want.clone(), want.clone()),
        "listing {prefix:?} in pages of {page}: expected {want:?}; page by page {:?}; streamed {:?}",
        got.0,
        got.1
    );
    Ok(())
}

pub(super) async fn list_page_sizes<S: ObjectStore>(
    store: &S,
    w: &Written,
    prefix: &KeyPrefix,
) -> CheckResult {
    let dir = child(prefix, "sizes/")?;
    let mut want = Vec::new();
    for i in 0..SIZED {
        let k = key(&dir, &format!("s{i}"))?;
        put(store, w, &k, &pattern(i + 1, 6)).await?;
        want.push((k.as_str().to_owned(), i as u64 + 1));
    }
    for page in SIZES {
        lists_exactly(store, &dir, page, &want).await?;
    }
    Ok(Outcome::Passed)
}

pub(super) async fn list_no_match<S: ObjectStore>(
    store: &S,
    w: &Written,
    prefix: &KeyPrefix,
) -> CheckResult {
    // Keys either side of `<prefix>nothing/` in key order, neither under it.
    put(store, w, &key(prefix, "nothing")?, &pattern(1, 1)).await?;
    put(store, w, &key(prefix, "nothinh")?, &pattern(1, 2)).await?;
    let dir = child(prefix, "nothing/")?;
    for page in [1, PageSize::MAX.get()] {
        lists_exactly(store, &dir, page, &[]).await?;
    }
    Ok(Outcome::Passed)
}

/// The names the nesting check writes, in no order: separators at several depths, and
/// the characters either side of `/` in byte order (`-` and `.` before it, `0`, `A`,
/// `a` and `~` after it). No name is another name followed by `/`: kbf never writes
/// such a pair, and MinIO does not list the keys under a key that is also an object
/// (`a` hides `a/b`).
const NESTED: [&str; 10] = [
    "a/b/c/d", "a~", "a/c", "A", "a/b/x", "a0", "a.b", "a/b/c/e", "B/x", "a-b",
];

pub(super) async fn list_nested<S: ObjectStore>(
    store: &S,
    w: &Written,
    prefix: &KeyPrefix,
) -> CheckResult {
    let dir = child(prefix, "nested/")?;
    let mut all = Vec::new();
    for name in NESTED {
        let k = key(&dir, name)?;
        put(store, w, &k, &pattern(1, 7)).await?;
        all.push(k);
    }
    // Byte order, which is S3's: `ObjectKey` orders by its text's bytes.
    all.sort();
    for (under, page) in [("", 3), ("a/", 2), ("a/b", 1)] {
        let within = child(&dir, under)?;
        let want: Vec<(String, u64)> = all
            .iter()
            .filter(|k| within.matches(k))
            .map(|k| (k.as_str().to_owned(), 1))
            .collect();
        lists_exactly(store, &within, page, &want).await?;
    }
    Ok(Outcome::Passed)
}

/// Keys present for the whole of the moving listing; keys it deletes once listed, one
/// after each page; and keys it deletes before its cursor reaches them.
const STEADY: [&str; 6] = ["b2", "k1", "k3", "k5", "k7", "k9"];
const BEHIND: [&str; 2] = ["b0", "b1"];
const AHEAD: [&str; 2] = ["k4", "k8"];

pub(super) async fn list_during_writes<S: ObjectStore>(
    store: &S,
    w: &Written,
    prefix: &KeyPrefix,
) -> CheckResult {
    let dir = child(prefix, "moving/")?;
    for name in STEADY.iter().chain(&BEHIND).chain(&AHEAD) {
        put(store, w, &key(&dir, name)?, &pattern(1, 8)).await?;
    }
    // After page n: a key after every steady key is written, and the n-th key already
    // listed and the n-th not yet listed are deleted. A failed delete leaves a key this
    // check allows either way, so its outcome is not checked.
    let writes = async |n: usize| {
        put(store, w, &key(&dir, &format!("z{n}"))?, &pattern(1, 8)).await?;
        for doomed in [BEHIND.get(n - 1), AHEAD.get(n - 1)].into_iter().flatten() {
            let _ = store.delete(&key(&dir, doomed)?).await;
        }
        Ok(())
    };
    let got = pages(store, &dir, 2, writes)
        .await
        .map_err(|e| format!("listing {dir:?} while keys were written and deleted: {e}"))?;
    // Written or deleted during the listing, a key may or may not be listed; every
    // key present throughout is listed exactly once, in order.
    let want: Vec<String> = STEADY
        .iter()
        .map(|s| format!("{}{s}", dir.as_str()))
        .collect();
    let steady: Vec<&String> = got
        .iter()
        .map(|(k, _)| k)
        .filter(|k| want.contains(k))
        .collect();
    ensure!(
        steady.iter().copied().eq(&want),
        "listing {dir:?} while keys were written and deleted: the keys present throughout came as {steady:?}, expected {want:?} (every key listed: {got:?})"
    );
    Ok(Outcome::Passed)
}
