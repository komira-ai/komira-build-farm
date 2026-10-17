//! `GET /v1/invocations`: the builds of the instances a caller may read, newest first.
//!
//! Each instance's list (`ui/inv/<instance>/<time>/<invocation>`) is one range scan;
//! with no `instance=` the scans of every instance the caller may read are merged. An
//! invocation that used several instances is listed under each and shown once, and only
//! when every instance it used is allowed (the reader checks its header).

use std::sync::Arc;

use axum::extract::State;
use axum::http::request::Parts;
use axum::response::Response;
use kbf_history::records::InvState;
use kbf_history::records::InvocationHeader;
use kbf_history::{Allow, IndexRoot, ListQuery, ListedInvocation, PageCursor};
use serde_json::{Value, json};

use super::{Cross, reply};
use crate::history::HistoryReads;
use crate::http_history::query::{Query, at_ms, bad};
use crate::http_history::scope::Scope;
use crate::http_history::unreadable;

const CALL: &str = "GET /v1/invocations";
const ROUTE: &str = "/v1/invocations";
const PARAMS: [&str; 8] = [
    "instance",
    "since",
    "until",
    "state",
    "tool",
    "commit",
    "page_size",
    "page_token",
];

pub(super) async fn page(State(cross): State<Arc<Cross>>, parts: Parts) -> Response {
    reply(read(&cross, &parts).await)
}

async fn read(cross: &Cross, parts: &Parts) -> Result<Value, Response> {
    let (readers, caller) = cross.begin(parts, CALL).await?;
    let q = Query::parse(parts.uri.query().unwrap_or_default(), &PARAMS)?;
    if q.get("commit").is_some() {
        return Err(bad(
            "needs_report",
            "commit= needs the commit a client reports for its build, which history does \
             not hold yet",
        ));
    }
    if q.get("tool").is_some() {
        return Err(bad(
            "unsupported_filter",
            "tool= is not kept: history holds the calls of a build, not the client tool's name",
        ));
    }
    let size = q.page_size()?;
    let since_ms = q.time_ms("since")?.unwrap_or(0);
    let until_ms = q.time_ms("until")?.unwrap_or(u64::MAX);
    let state = q.state()?;
    let after = match q.position(ROUTE)? {
        None => None,
        Some(text) => Some(
            text.parse::<PageCursor>()
                .map_err(|_| bad("bad_page_token", "the page token holds no position"))?,
        ),
    };
    let mut scope = Scope::new(readers, &caller);
    let (instances, partial) = cross.reach(&mut scope, &q).await?;
    // Every instance asked about, not only those listed: an invocation shows when all the
    // instances it used are allowed.
    let allow: Allow<'_> = &|instance: &str| scope.allows(instance);
    let filter = Listing {
        since_ms,
        until_ms,
        state,
        size,
        after: after.as_ref(),
    };
    let reads = &cross.durable.reads;
    let Merged { items, next } = merged(reads, &instances, &filter, allow).await?;
    let mut answer =
        json!({"invocations": items.iter().map(|i| item(&i.header)).collect::<Vec<_>>()});
    if let Some(next) = next {
        answer["next_page"] = q.next_page(ROUTE, size, &q.token(ROUTE, &next)).into();
    }
    if partial {
        answer["partial"] = true.into();
    }
    Ok(answer)
}

/// What a page of the list is asked for besides the instances.
pub(super) struct Listing<'a> {
    pub(super) since_ms: u64,
    pub(super) until_ms: u64,
    pub(super) state: Option<InvState>,
    pub(super) size: usize,
    pub(super) after: Option<&'a PageCursor>,
}

/// A page of the merged lists of several instances.
pub(super) struct Merged {
    /// The invocations, newest first, one each.
    pub(super) items: Vec<ListedInvocation>,
    /// Where the next page starts, as the text a page token carries; `None` after the
    /// last.
    pub(super) next: Option<String>,
}

/// The newest `filter.size` invocations of `instances` after `filter.after`, merged and
/// each shown once; `allow` accepts the instances every invocation listed used.
pub(super) async fn merged(
    reads: &HistoryReads,
    instances: &[String],
    filter: &Listing<'_>,
    allow: Allow<'_>,
) -> Result<Merged, Response> {
    let queries: Vec<ListQuery<'_>> = instances
        .iter()
        .map(|instance| ListQuery {
            instance,
            since_ms: filter.since_ms,
            until_ms: filter.until_ms,
            state: filter.state,
            limit: filter.size,
            after: filter.after,
        })
        .collect();
    let pages = futures::future::try_join_all(
        queries
            .iter()
            .map(|query| reads.list_invocations(query, allow)),
    )
    .await
    .map_err(unreadable)?;
    // What every instance's scan has covered: the newest of the cursors the scans stopped
    // at. Rows older than it may be unread in some instance, so they wait for the next
    // page; it is where the next page starts when this one holds nothing past it.
    let covered = pages
        .iter()
        .filter_map(|p| p.next.as_ref().map(ToString::to_string))
        .max();
    let mut merged: Vec<(String, ListedInvocation)> = pages
        .into_iter()
        .flat_map(|p| p.items)
        .map(|item| {
            (
                IndexRoot::inv_list_tail(item.header.list_ms, &item.inv),
                item,
            )
        })
        .filter(|(tail, _)| covered.as_ref().is_none_or(|c| tail >= c))
        .collect();
    merged.sort_by(|a, b| b.0.cmp(&a.0));
    // One entry per invocation: the same invocation listed under two instances has the
    // same key time and so the same tail.
    merged.dedup_by(|a, b| a.0 == b.0);
    let more = merged.len() > filter.size;
    merged.truncate(filter.size);
    let next = if more {
        merged.last().map(|(tail, _)| tail.clone())
    } else {
        covered
    };
    Ok(Merged {
        items: merged.into_iter().map(|(_, item)| item).collect(),
        next,
    })
}

/// One invocation of the list, from its header.
fn item(h: &InvocationHeader) -> Value {
    json!({
        "id": h.record_id,
        "first_seen": at_ms(h.first_seen_ms),
        "last_seen": at_ms(h.last_seen_ms),
        "state": if h.state() == InvState::Closed { "closed" } else { "open" },
        "complete": h.state() == InvState::Closed && h.dropped == 0,
        "counts": {"ran": h.runs, "hits": h.hits, "failed": h.failed},
        "instances": h.instances,
        "href": format!("/v1/invocations/{}", h.record_id),
    })
}
