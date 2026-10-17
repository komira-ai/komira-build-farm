//! Every object under a prefix, as a stream that holds one page at a time.
//!
//! [`objects`] calls [`ObjectStore::list`] page by page and yields each object in
//! ascending key order, so a listing of any size needs memory for one page (at most
//! [`PageSize::MAX`] objects), never for the whole prefix. It checks what it is given:
//! a key outside the prefix, a key not greater than the one before it (a page served
//! twice, or a store that does not list in key order), or an empty page whose token is
//! the one it was asked with (a store that would page forever) ends the stream with
//! [`ObjectStoreError::Protocol`]. Any error from the store ends the stream with that
//! error; the stream yields nothing after an error.
//!
//! A listing is not a snapshot. Objects written or deleted while it runs may or may not
//! be listed; every object present from the first page to the last is listed exactly
//! once. To resume a listing that stopped, start a new one with
//! [`ListToken::after`] the last key handled: order is by key, so the new listing
//! starts where the old one stopped.

use futures::{Stream, TryStreamExt as _, stream};

use crate::{KeyPrefix, ListToken, ObjectInfo, ObjectKey, ObjectStore, ObjectStoreError, PageSize};

/// Every object under `prefix`, in ascending key order, starting where `from` says (or
/// at the first key), fetched `page` keys at a time.
pub fn objects<'a, S: ObjectStore>(
    store: &'a S,
    prefix: &'a KeyPrefix,
    from: Option<ListToken>,
    page: PageSize,
) -> impl Stream<Item = Result<ObjectInfo, ObjectStoreError>> + Send + 'a {
    let start = Cursor {
        // A listing after a key checks its first key against that one.
        last: from.as_ref().and_then(|t| t.local_after().ok()).cloned(),
        token: from,
        done: false,
    };
    stream::try_unfold(start, move |cursor| async move {
        if cursor.done {
            return Ok(None);
        }
        let got = store.list(prefix, cursor.token.as_ref(), page).await?;
        let objects = checked(prefix, cursor.last.as_ref(), got.objects)?;
        if objects.is_empty() && got.next.is_some() && got.next == cursor.token {
            return Err(ObjectStoreError::Protocol(format!(
                "listing {:?}: an empty page whose token is the one it was asked with",
                prefix.as_str()
            )));
        }
        let next = Cursor {
            last: objects.last().map(|o| o.key.clone()).or(cursor.last),
            done: got.next.is_none(),
            token: got.next,
        };
        Ok(Some((stream::iter(objects.into_iter().map(Ok)), next)))
    })
    .try_flatten()
}

/// Where the next page of [`objects`] starts.
struct Cursor {
    /// The token to ask with: `None` on the first page of a listing from the start.
    token: Option<ListToken>,
    /// The last key yielded, which every key on the next page must follow.
    last: Option<ObjectKey>,
    /// The last page has been read.
    done: bool,
}

/// `page`, once every key is checked to be under `prefix` and greater than the key
/// before it (the first, greater than `last`).
fn checked(
    prefix: &KeyPrefix,
    last: Option<&ObjectKey>,
    page: Vec<ObjectInfo>,
) -> Result<Vec<ObjectInfo>, ObjectStoreError> {
    for (i, object) in page.iter().enumerate() {
        if !prefix.matches(&object.key) {
            return Err(ObjectStoreError::Protocol(format!(
                "listing {:?} returned {}, which is outside it",
                prefix.as_str(),
                object.key
            )));
        }
        let before = match i.checked_sub(1) {
            Some(j) => Some(&page[j].key),
            None => last,
        };
        if let Some(before) = before.filter(|before| object.key <= **before) {
            return Err(ObjectStoreError::Protocol(format!(
                "listing {:?} returned {} after {before}: keys must ascend",
                prefix.as_str(),
                object.key
            )));
        }
    }
    Ok(page)
}
