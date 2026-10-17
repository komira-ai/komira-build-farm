//! The cross-build reads: the invocation list, the shape catalog and the tests by
//! status, over the `ui/inv`, `ui/shape`, `ui/status` and `ui/statusx` rows.
//!
//! Every read is a bounded range scan (C8), never a scan of a whole prefix: a list of
//! one instance grows to hundreds of thousands of rows. A page scans at most
//! [`MAX_SCAN`] rows (a reader may be set lower) and answers a cursor to resume from,
//! so a filter that matches little gives a short page, or an empty one, and a cursor,
//! not a long wait. A cursor is the tail of the last row's key: it stays valid across
//! commits and restarts, and rows arriving newer than a list's cursor do not move its
//! pages.
//!
//! Every answer holds only what `allow` accepts. An instance's own rows are answered
//! only when `allow` accepts the instance, and a listed invocation only when `allow`
//! accepts every instance its header lists (ui-data.md section 8).

use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::ops::Bound;
use std::str::FromStr;

use kbf_state::Direction;
use prost::Message;

use crate::keys::{IndexRoot, InvKey};
use crate::reader::{ReadError, Reader, all_allow, decode_row};
use crate::records::{InvState, InvocationHeader};
use crate::rows::{ShapeRow, StatusKind, StatusRow};

/// Rows of an index list a page answers at most.
pub const MAX_PAGE: usize = 200;

/// Index rows a page of a list scans at most, matching or not.
pub const MAX_SCAN: usize = 10_000;

/// The largest time a list key holds (twelve hex digits), milliseconds.
const MAX_TIME: u64 = 0xffff_ffff_ffff;

/// Rows one range scan asks for.
const CHUNK: usize = 256;

/// Instance names [`Reader::instances`] answers at most.
const MAX_INSTANCE_NAMES: usize = 256;

/// The longest cursor text accepted.
const MAX_CURSOR: usize = 1024;

/// Where a page of a list starts: after the row named by this text, which is the tail of
/// the row's key. Opaque to callers, who pass back what a page gave them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageCursor(String);

impl fmt::Display for PageCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for PageCursor {
    type Err = PageCursorError;

    fn from_str(text: &str) -> Result<PageCursor, PageCursorError> {
        if text.is_empty() || text.len() > MAX_CURSOR || text.chars().any(char::is_control) {
            return Err(PageCursorError);
        }
        Ok(PageCursor(text.to_owned()))
    }
}

/// Text that is not a page cursor: empty, over 1,024 bytes, or with a control character.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a page cursor (1 to 1024 bytes, no control characters)")]
pub struct PageCursorError;

/// What a page of invocations is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListQuery<'a> {
    /// The instance whose list is read.
    pub instance: &'a str,
    /// Invocations first listed at or after this time, milliseconds.
    pub since_ms: u64,
    /// Invocations first listed at or before this time, milliseconds.
    pub until_ms: u64,
    /// Only invocations in this state.
    pub state: Option<InvState>,
    /// Invocations a page holds at most (1 to [`MAX_PAGE`]).
    pub limit: usize,
    /// Where the page starts, from the page before.
    pub after: Option<&'a PageCursor>,
}

impl<'a> ListQuery<'a> {
    /// The newest `limit` invocations of `instance`, whatever their time and state.
    #[must_use]
    pub fn new(instance: &'a str, limit: usize) -> ListQuery<'a> {
        ListQuery {
            instance,
            since_ms: 0,
            until_ms: u64::MAX,
            state: None,
            limit,
            after: None,
        }
    }
}

/// An invocation of a list.
#[derive(Clone, Debug, PartialEq)]
pub struct ListedInvocation {
    /// Its key.
    pub inv: InvKey,
    /// Its header row; `list_ms` is the time it is listed by.
    pub header: InvocationHeader,
}

/// A page of invocations, newest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InvocationPage {
    /// The invocations.
    pub items: Vec<ListedInvocation>,
    /// Where the next page starts. A page may be short, or empty, with a cursor: the
    /// scan stopped at its bound. `None` after the last row.
    pub next: Option<PageCursor>,
}

/// The instances that have lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstanceList {
    /// The names `allow` accepts, in key order.
    pub names: Vec<String>,
    /// More instances exist than the bound reads.
    pub truncated: bool,
}

/// A shape catalog row with the key it is under.
#[derive(Clone, Debug, PartialEq)]
pub struct ShapeEntry {
    /// The instance.
    pub instance: String,
    /// The shape key.
    pub shape: u64,
    /// The configuration.
    pub cfg: String,
    /// The row.
    pub row: ShapeRow,
}

/// A page of catalog rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShapePage {
    /// The rows, in shape order.
    pub items: Vec<ShapeEntry>,
    /// Where the next page starts; `None` after the last row.
    pub next: Option<PageCursor>,
}

/// A status row with the key it is under.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusEntry {
    /// The instance.
    pub instance: String,
    /// The shape key.
    pub shape: u64,
    /// The configuration.
    pub cfg: String,
    /// The row.
    pub row: StatusRow,
}

/// A page of status rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusPage {
    /// The rows, in instance and shape order.
    pub items: Vec<StatusEntry>,
    /// Where the next page starts. A page may be short, or empty, with a cursor.
    pub next: Option<PageCursor>,
}

/// A bounded range: the rows of `prefix` between the bounds, walked in `direction`.
struct Walk {
    prefix: String,
    lower: Bound<String>,
    upper: Bound<String>,
    direction: Direction,
}

impl Walk {
    /// The same walk, starting past the row `after` names (a key tail).
    fn after(mut self, after: Option<&PageCursor>) -> Walk {
        let Some(after) = after else {
            return self;
        };
        let key = format!("{}{after}", self.prefix);
        self.move_past(key);
        self
    }

    /// Moves the start of the walk past the row `key` (a full key), in its direction.
    fn move_past(&mut self, key: String) {
        let past = Bound::Excluded(key);
        match self.direction {
            Direction::Forward => self.lower = past,
            Direction::Reverse => self.upper = past,
        }
    }
}

/// The time part of a list key's bound: `<time>/` sorts before every entry of that time.
fn at(prefix: &str, ms: u64) -> String {
    format!("{prefix}{:012x}/", ms.min(MAX_TIME))
}

impl<O> Reader<O> {
    /// A page of the invocations listed under `q.instance`, newest first: one reverse
    /// range scan of `ui/inv/<instance>/` between the times, then each row's header,
    /// which must name the row's time (a list entry the header does not name is stale),
    /// list only instances `allow` accepts and be in the state asked for.
    ///
    /// # Errors
    /// [`ReadError`] when the index fails or a header row does not decode.
    pub async fn list_invocations(
        &self,
        q: &ListQuery<'_>,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<InvocationPage, ReadError> {
        if !allow(q.instance) {
            return Ok(InvocationPage::default());
        }
        let root = self.index.root();
        let prefix = root.inv_list_prefix(q.instance);
        let upper = match q.until_ms.checked_add(1) {
            Some(after) if after <= MAX_TIME => Bound::Excluded(at(&prefix, after)),
            _ => Bound::Unbounded,
        };
        let walk = Walk {
            lower: Bound::Included(at(&prefix, q.since_ms)),
            upper,
            direction: Direction::Reverse,
            prefix,
        };
        let resolve = |key: String| async move {
            let Some((_, listed, inv)) = root.parse_inv_list(&key) else {
                return Ok(None);
            };
            let row = root.invid(&inv);
            let Some(stored) = self.index.get(row.clone()).await? else {
                return Ok(None);
            };
            let header = decode_row::<InvocationHeader>(&row, &stored.value)?;
            let wanted = header.list_ms == listed
                && !header.idless
                && all_allow(&header, allow)
                && q.state.is_none_or(|s| header.state() == s);
            Ok(wanted.then_some(ListedInvocation { inv, header }))
        };
        let (items, next) = self
            .scan_matches(walk.after(q.after), q.limit, resolve)
            .await?;
        Ok(InvocationPage { items, next })
    }

    /// The instances that have invocation lists and `allow` accepts, by skipping from
    /// one instance's rows to the next with one-row scans: at most 256 instances are
    /// read, and `truncated` says there were more.
    ///
    /// # Errors
    /// [`ReadError`] when the index fails.
    pub async fn instances(&self, allow: &dyn Fn(&str) -> bool) -> Result<InstanceList, ReadError> {
        let prefix = self.index.root().inv_list_root();
        let mut out = InstanceList::default();
        let mut lower = Bound::Unbounded;
        let mut seen = 0;
        loop {
            let bounds = (lower, Bound::Unbounded);
            let one = NonZeroUsize::MIN;
            let page = self
                .index
                .scan_range(prefix.clone(), bounds, Direction::Forward, one)
                .await?;
            let Some((key, _)) = page.rows.first() else {
                return Ok(out);
            };
            if seen == MAX_INSTANCE_NAMES {
                out.truncated = true;
                return Ok(out);
            }
            seen += 1;
            let rest = key.strip_prefix(prefix.as_str()).unwrap_or(key);
            let segment = rest.split('/').next().unwrap_or(rest);
            // Every key of the instance is below `<segment>0` ('0' follows '/'), so the
            // next scan starts at its first key past them.
            lower = Bound::Included(format!("{prefix}{segment}0"));
            if let Some(name) = IndexRoot::unsegment(segment).filter(|n| allow(n)) {
                out.names.push(name);
            }
        }
    }

    /// A page of `instance`'s catalog rows in shape order, if `allow` accepts the
    /// instance.
    ///
    /// # Errors
    /// [`ReadError`] when the index fails or a row does not decode.
    pub async fn shapes(
        &self,
        instance: &str,
        after: Option<&PageCursor>,
        limit: usize,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<ShapePage, ReadError> {
        if !allow(instance) {
            return Ok(ShapePage::default());
        }
        let owned = self.index.root().shape_prefix(instance);
        let prefix = owned.as_str();
        let walk = Walk {
            prefix: owned.clone(),
            lower: Bound::Unbounded,
            upper: Bound::Unbounded,
            direction: Direction::Forward,
        };
        let resolve = |key: String| async move {
            let stored = self.index.get(key.clone()).await?;
            let tail = key
                .strip_prefix(prefix)
                .and_then(IndexRoot::parse_shape_tail);
            let (Some(stored), Some((shape, cfg))) = (stored, tail) else {
                return Ok(None);
            };
            let row = decode_row::<ShapeRow>(&key, &stored.value)?;
            Ok(Some(ShapeEntry {
                instance: instance.to_owned(),
                shape,
                cfg,
                row,
            }))
        };
        let (items, next) = self.scan_matches(walk.after(after), limit, resolve).await?;
        Ok(ShapePage { items, next })
    }

    /// The catalog row of (`instance`, `shape`, `cfg`), if there is one and `allow`
    /// accepts the instance.
    ///
    /// # Errors
    /// [`ReadError`] when the index fails or the row does not decode.
    pub async fn shape(
        &self,
        instance: &str,
        shape: u64,
        cfg: &str,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<Option<ShapeEntry>, ReadError> {
        let key = self.index.root().shape(instance, shape, cfg);
        let row = self.row::<ShapeRow>(key, instance, allow).await?;
        Ok(row.map(|row| ShapeEntry {
            instance: instance.to_owned(),
            shape,
            cfg: cfg.to_owned(),
            row,
        }))
    }

    /// The status row of the test (`instance`, `shape`, `cfg`), if there is one and
    /// `allow` accepts the instance.
    ///
    /// # Errors
    /// [`ReadError`] when the index fails or the row does not decode.
    pub async fn status(
        &self,
        instance: &str,
        shape: u64,
        cfg: &str,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<Option<StatusEntry>, ReadError> {
        let key = self.index.root().status(instance, shape, cfg);
        let row = self.row::<StatusRow>(key, instance, allow).await?;
        Ok(row.map(|row| StatusEntry {
            instance: instance.to_owned(),
            shape,
            cfg: cfg.to_owned(),
            row,
        }))
    }

    /// A page of the tests whose status is `kind`, by instance and shape: one forward
    /// scan of `ui/statusx/<status>/`, then each row's status row, which is read only for
    /// an instance `allow` accepts; a row whose status row is gone or says another
    /// status (a move in flight) is left out.
    ///
    /// # Errors
    /// [`ReadError`] when the index fails or a status row does not decode.
    pub async fn by_status(
        &self,
        kind: StatusKind,
        after: Option<&PageCursor>,
        limit: usize,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<StatusPage, ReadError> {
        let root = self.index.root();
        let owned = root.statusx_prefix(kind.name());
        let prefix = owned.as_str();
        let walk = Walk {
            prefix: owned.clone(),
            lower: Bound::Unbounded,
            upper: Bound::Unbounded,
            direction: Direction::Forward,
        };
        let resolve = |key: String| async move {
            let tail = key.strip_prefix(prefix);
            let named = tail.and_then(|t| t.split_once('/'));
            let named = named.and_then(|(i, rest)| {
                let instance = IndexRoot::unsegment(i)?;
                let (shape, cfg) = IndexRoot::parse_shape_tail(rest)?;
                Some((instance, shape, cfg))
            });
            let Some((instance, shape, cfg)) = named else {
                return Ok(None);
            };
            let entry = self.status(&instance, shape, &cfg, allow).await?;
            Ok(entry.filter(|e| e.row.kind() == Some(kind)))
        };
        let (items, next) = self.scan_matches(walk.after(after), limit, resolve).await?;
        Ok(StatusPage { items, next })
    }

    /// The row `key` of `instance` decoded as `M`, if it exists and `allow` accepts the
    /// instance.
    async fn row<M: Message + Default>(
        &self,
        key: String,
        instance: &str,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<Option<M>, ReadError> {
        if !allow(instance) {
            return Ok(None);
        }
        let stored = self.index.get(key.clone()).await?;
        stored.map(|v| decode_row::<M>(&key, &v.value)).transpose()
    }

    /// The first `limit` (1 to [`MAX_PAGE`]) items `resolve` makes of the rows of
    /// `walk`, and the cursor to resume from. Rows are scanned [`CHUNK`] at a time and
    /// resolved together; the scan stops after [`Reader::scanning_at_most`] rows with a
    /// cursor at the last row scanned, so an empty page may have one.
    async fn scan_matches<T, F, Fut>(
        &self,
        mut walk: Walk,
        limit: usize,
        resolve: F,
    ) -> Result<(Vec<T>, Option<PageCursor>), ReadError>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Result<Option<T>, ReadError>>,
    {
        let limit = limit.clamp(1, MAX_PAGE);
        let cursor = |walk: &Walk, key: &str| {
            let tail = key.strip_prefix(walk.prefix.as_str()).unwrap_or(key);
            PageCursor(tail.to_owned())
        };
        let mut out = Vec::new();
        let mut scanned = 0;
        loop {
            let room = self.scan_budget.saturating_sub(scanned).min(CHUNK);
            let chunk = NonZeroUsize::new(room).unwrap_or(NonZeroUsize::MIN);
            let bounds = (walk.lower.clone(), walk.upper.clone());
            let page = self
                .index
                .scan_range(walk.prefix.clone(), bounds, walk.direction, chunk)
                .await?;
            scanned += page.rows.len();
            let keys: Vec<String> = page.rows.into_iter().map(|(key, _)| key).collect();
            let found = futures::future::try_join_all(keys.iter().cloned().map(&resolve)).await?;
            for (at, (key, item)) in keys.iter().zip(found).enumerate() {
                let Some(item) = item else {
                    continue;
                };
                out.push(item);
                if out.len() == limit {
                    let more = at + 1 < keys.len() || page.resume.is_some();
                    return Ok((out, more.then(|| cursor(&walk, key))));
                }
            }
            let Some(resume) = page.resume else {
                return Ok((out, None));
            };
            if scanned >= self.scan_budget {
                return Ok((out, Some(cursor(&walk, resume.key()))));
            }
            walk.move_past(resume.key().to_owned());
        }
    }
}
