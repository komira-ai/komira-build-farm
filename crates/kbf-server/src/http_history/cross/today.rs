//! `GET /v1/actions/{digest}/runs` and `GET /v1/runs?node=`: the runs of an action digest,
//! and the runs that overlapped a window on a node, from the writer's bounded index of
//! today's runs ([`kbf_history::Today`]).
//!
//! Neither answers a closed day: that needs the day index the day-close fold writes, which
//! does not run yet. An answer says so: `"partial": true`, and `covers_from`, the time from
//! which the index has every run it was sent (the writer's start, or the oldest run it
//! still holds). Instance scoping is per run: a run of an instance the caller may not read
//! is never listed, and the authorizer is asked once per request about each instance the
//! index holds.

#![allow(
    clippy::result_large_err,
    reason = "the Err is the answer to send, built once on a refusal; boxing it would only \
              move it"
)]

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::response::Response;
use kbf_history::records::RunOutcome;
use kbf_history::{ActionKey, Allow, TodayPage, TodayQuery, TodayRun};
use serde_json::{Value, json};

use super::{Cross, reply};
use crate::http_blobs::refusal;
use crate::http_history::query::{Query, at_ms, bad};
use crate::http_history::run::class;
use crate::http_history::scope::Scope;

const DIGEST_ROUTE: &str = "/v1/actions/{digest}/runs";
const NODE_ROUTE: &str = "/v1/runs";

/// The classes a run ends in, in the order of the enum.
const CLASSES: [RunOutcome; 8] = [
    RunOutcome::Unspecified,
    RunOutcome::Ok,
    RunOutcome::Failed,
    RunOutcome::TimedOut,
    RunOutcome::MemoryKilled,
    RunOutcome::Infra,
    RunOutcome::Lost,
    RunOutcome::Cancelled,
];

pub(super) async fn of_action(
    State(cross): State<Arc<Cross>>,
    Path(text): Path<String>,
    parts: Parts,
) -> Response {
    reply(action_runs(&cross, &text, &parts).await)
}

pub(super) async fn on_node(State(cross): State<Arc<Cross>>, parts: Parts) -> Response {
    reply(node_runs(&cross, &parts).await)
}

/// The digest `text` is: the hash as 64 lowercase hex digits, a `-` and the size in bytes.
fn parse_digest(text: &str) -> Result<ActionKey, Response> {
    let not = || {
        bad(
            "bad_request",
            &format!("{text:?} is not a digest: 64 lowercase hex digits, a `-` and the size"),
        )
    };
    let (hash, size) = text.split_once('-').ok_or_else(not)?;
    let lower = hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let hash = hex::decode(hash).ok().filter(|h| lower && h.len() == 32);
    let size = size.parse::<u64>().ok().filter(|s| s.to_string() == size);
    hash.zip(size).ok_or_else(not)
}

/// `GET /v1/actions/{digest}/runs`.
async fn action_runs(cross: &Cross, text: &str, parts: &Parts) -> Result<Value, Response> {
    let (readers, caller) = cross.begin(parts, DIGEST_ROUTE).await?;
    let action = parse_digest(text)?;
    let q = Query::parse(
        parts.uri.query().unwrap_or_default(),
        &["instance", "class", "page_size", "page_token"],
    )?;
    let (size, after, class) = paging(&q, DIGEST_ROUTE)?;
    let mut scope = Scope::new(readers, &caller);
    let today = cross.durable.reads.today();
    ask(cross, &mut scope, &q).await?;
    let allow: Allow<'_> = &|instance: &str| scope.allows(instance);
    let query = TodayQuery {
        instance: q.get("instance"),
        class,
        limit: size,
        after,
        ..TodayQuery::newest(size)
    };
    let page = today.of_action(&action, &query, allow);
    let mut answer = render(
        &page,
        &q,
        size,
        &format!("/v1/actions/{text}/runs"),
        DIGEST_ROUTE,
    );
    answer["digest"] = text.into();
    Ok(answer)
}

/// `GET /v1/runs?node=&from=&to=&class=&instance=`.
async fn node_runs(cross: &Cross, parts: &Parts) -> Result<Value, Response> {
    let (readers, caller) = cross.begin(parts, NODE_ROUTE).await?;
    let q = Query::parse(
        parts.uri.query().unwrap_or_default(),
        &[
            "node",
            "from",
            "to",
            "class",
            "instance",
            "page_size",
            "page_token",
        ],
    )?;
    let Some(node) = q.get("node") else {
        return Err(bad(
            "bad_request",
            "node is required: the node whose runs are listed",
        ));
    };
    let from_ms = q.time_ms("from")?.unwrap_or(0);
    let to_ms = q.time_ms("to")?.unwrap_or(u64::MAX);
    if from_ms > to_ms {
        return Err(bad("bad_request", "from is after to"));
    }
    let (size, after, class) = paging(&q, NODE_ROUTE)?;
    let mut scope = Scope::new(readers, &caller);
    let today = cross.durable.reads.today();
    ask(cross, &mut scope, &q).await?;
    let allow: Allow<'_> = &|instance: &str| scope.allows(instance);
    let query = TodayQuery {
        instance: q.get("instance"),
        class,
        from_ms,
        to_ms,
        limit: size,
        after,
    };
    let page = today.on_node(node, &query, allow);
    Ok(render(&page, &q, size, NODE_ROUTE, NODE_ROUTE))
}

/// The page size, the position and the class a request asks for.
fn paging(q: &Query, route: &str) -> Result<(usize, Option<u64>, Option<RunOutcome>), Response> {
    let size = q.page_size()?;
    let after = match q.position(route)? {
        None => None,
        Some(text) => Some(
            text.parse::<u64>()
                .map_err(|_| bad("bad_page_token", "the page token holds no position"))?,
        ),
    };
    let wanted = match q.get("class") {
        None => None,
        Some(name) => Some(
            CLASSES
                .into_iter()
                .find(|c| class(*c) == name)
                .ok_or_else(|| bad("bad_request", &format!("class={name:?}: no such class")))?,
        ),
    };
    Ok((size, after, wanted))
}

/// Asks the authorizer about the instance named in the query, which must be allowed, and
/// every instance the index holds, once.
async fn ask(cross: &Cross, scope: &mut Scope<'_>, q: &Query) -> Result<(), Response> {
    let held = cross.durable.reads.today().instances();
    let named = q.get("instance");
    let asking: Vec<&str> = held.iter().map(String::as_str).chain(named).collect();
    scope.ask(&asking).await;
    match named.and_then(|n| scope.refusal(n)) {
        Some(why) => Err(refusal(why)),
        None => Ok(()),
    }
}

/// The answer for `page`: the runs, that it is partial and from when it covers.
fn render(page: &TodayPage, q: &Query, size: usize, path: &str, route: &str) -> Value {
    let mut answer = json!({
        "runs": page.runs.iter().map(item).collect::<Vec<_>>(),
        "partial": true,
    });
    if let Some(ms) = page.covers_from_ms {
        answer["covers_from"] = at_ms(ms);
    }
    if let Some(next) = page.next {
        answer["next_page"] = q
            .next_page(path, size, &q.token(route, &next.to_string()))
            .into();
    }
    answer
}

/// One run of an answer.
fn item(r: &TodayRun) -> Value {
    let id = r.run_id.to_string();
    json!({
        "run_id": id,
        "run_href": format!("/v1/runs/{id}"),
        "instance": r.instance,
        "worker": r.node,
        "class": class(r.outcome),
        "started": at_ms(r.started_ms),
        "ended": at_ms(r.ended_ms),
        "shape": (r.shape != 0).then(|| json!({"key": format!("{:016x}", r.shape), "cfg": r.cfg})),
    })
}

#[cfg(test)]
#[path = "today_tests.rs"]
mod tests;
