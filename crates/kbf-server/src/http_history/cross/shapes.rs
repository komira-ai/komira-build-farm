//! `GET /v1/shapes`, `/v1/tests`, `/v1/shapes/{key}` and `/v1/shapes/{key}/runs`: the
//! shape catalog and what the runs of each test say of it, from the `ui/shape`,
//! `ui/status` and `ui/statusx` rows.
//!
//! A row is one `(instance, shape key, configuration)`. A test has a status row (its last
//! outcome, the flaky proof and a ring of its last 16 runs); a shape that is not a test
//! has the catalog row only. Nothing here reads an object.

#![allow(
    clippy::result_large_err,
    reason = "the Err is the answer to send, built once on a refusal; boxing it would only \
              move it"
)]

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::response::Response;
use kbf_history::records::TestStatus;
use kbf_history::{
    Allow, Mark, PageCursor, ReadError, RingRun, ShapeEntry, ShapeKind, ShapeRow, StatusKind,
    StatusRow,
};
use serde_json::{Map, Value, json};

use super::{Cross, reply};
use crate::http_history::query::{Query, at_ms, bad, missing};
use crate::http_history::scope::Scope;
use crate::http_history::unreadable;

/// Reads of the index one request makes at most, for one page of a list.
const MAX_FETCHES: usize = 16;

const LIST_PARAMS: [&str; 7] = [
    "kind",
    "status",
    "instance",
    "target",
    "program",
    "page_size",
    "page_token",
];

/// Everything one row of an answer says: its catalog row and, for a test, its status.
struct Found {
    instance: String,
    shape: u64,
    cfg: String,
    catalog: Option<ShapeRow>,
    status: Option<StatusRow>,
}

pub(super) async fn shapes(State(cross): State<Arc<Cross>>, parts: Parts) -> Response {
    reply(list(&cross, &parts, "/v1/shapes", false).await)
}

pub(super) async fn tests(State(cross): State<Arc<Cross>>, parts: Parts) -> Response {
    reply(list(&cross, &parts, "/v1/tests", true).await)
}

pub(super) async fn one(
    State(cross): State<Arc<Cross>>,
    Path(key): Path<String>,
    parts: Parts,
) -> Response {
    reply(detail(&cross, &key, &parts).await)
}

pub(super) async fn runs(
    State(cross): State<Arc<Cross>>,
    Path(key): Path<String>,
    parts: Parts,
) -> Response {
    reply(ring(&cross, &key, &parts).await)
}

/// `GET /v1/shapes` or `/v1/tests`.
async fn list(
    cross: &Cross,
    parts: &Parts,
    route: &'static str,
    tests_only: bool,
) -> Result<Value, Response> {
    let (readers, caller) = cross.begin(parts, route).await?;
    let q = Query::parse(parts.uri.query().unwrap_or_default(), &LIST_PARAMS)?;
    let kind = match q.get("kind") {
        None => tests_only.then_some(ShapeKind::Test),
        Some("test") => Some(ShapeKind::Test),
        Some("other") if !tests_only => Some(ShapeKind::Other),
        Some(other) => {
            return Err(bad(
                "bad_request",
                &format!(
                    "kind={other:?}: test{}",
                    if tests_only { "" } else { " or other" }
                ),
            ));
        }
    };
    for name in ["target", "program"] {
        if q.get(name).is_some() {
            return Err(bad(
                "unsupported_filter",
                &format!("{name}= needs the catalog's body, which history does not keep yet"),
            ));
        }
    }
    let status = match q.get("status") {
        None => None,
        Some("slower") => {
            return Err(bad(
                "unsupported_status",
                "status=slower compares a test's last 7 days of run times, which needs the \
                 day-close fold history does not run yet",
            ));
        }
        Some(name) => Some(StatusKind::parse(name).ok_or_else(|| {
            bad(
                "bad_request",
                &format!("status={name:?}: one of passing, failing, flaky, oom_killed"),
            )
        })?),
    };
    if status.is_some() && kind == Some(ShapeKind::Other) {
        return Err(bad("bad_request", "only a test has a status"));
    }
    let size = q.page_size()?;
    let position = q.position(route)?;
    let mut scope = Scope::new(readers, &caller);
    let (instances, partial) = cross.reach(&mut scope, &q).await?;
    let reach: BTreeSet<&str> = instances.iter().map(String::as_str).collect();
    let allow: Allow<'_> = &|instance: &str| reach.contains(instance);
    let reads = &cross.durable.reads;
    let (found, next) = if let Some(status) = status {
        let after = position.as_deref().map(parse_cursor).transpose()?;
        let page = reads
            .by_status(status, after.as_ref(), size, allow)
            .await
            .map_err(unreadable)?;
        let found = futures::future::try_join_all(page.items.into_iter().map(|e| async move {
            let catalog = reads.shape(&e.instance, e.shape, &e.cfg, allow).await?;
            Ok::<_, ReadError>(Found {
                instance: e.instance,
                shape: e.shape,
                cfg: e.cfg,
                catalog: catalog.map(|c| c.row),
                status: Some(e.row),
            })
        }))
        .await
        .map_err(unreadable)?;
        (found, page.next.map(|c| c.to_string()))
    } else {
        let start = position.as_deref().map(decode_position).transpose()?;
        let (entries, next) = across(
            &instances,
            start,
            size,
            |instance, after, room| async move {
                let page = reads.shapes(&instance, after.as_ref(), room, allow).await?;
                let keep = |e: &ShapeEntry| kind.is_none_or(|k| e.row.kind() == k);
                Ok::<_, ReadError>((page.items.into_iter().filter(keep).collect(), page.next))
            },
        )
        .await?;
        (join_status(cross, entries, allow).await?, next)
    };
    let mut answer = json!({"shapes": found.iter().map(render).collect::<Vec<_>>()});
    if let Some(next) = next {
        answer["next_page"] = q.next_page(route, size, &q.token(route, &next)).into();
    }
    if partial {
        answer["partial"] = true.into();
    }
    Ok(answer)
}

/// `GET /v1/shapes/{key}`: one `(instance, cfg)`, or the rows of the key.
async fn detail(cross: &Cross, key: &str, parts: &Parts) -> Result<Value, Response> {
    const ROUTE: &str = "/v1/shapes/{key}";
    let (readers, caller) = cross.begin(parts, ROUTE).await?;
    let shape = parse_key(key)?;
    let q = Query::parse(
        parts.uri.query().unwrap_or_default(),
        &["instance", "cfg", "page_size", "page_token"],
    )?;
    let size = q.page_size()?;
    let position = q.position(ROUTE)?;
    let mut scope = Scope::new(readers, &caller);
    let (instances, partial) = cross.reach(&mut scope, &q).await?;
    let reach: BTreeSet<&str> = instances.iter().map(String::as_str).collect();
    let allow: Allow<'_> = &|instance: &str| reach.contains(instance);
    let reads = &cross.durable.reads;
    if let (Some(instance), Some(cfg)) = (q.get("instance"), q.get("cfg")) {
        let entry = reads
            .shape(instance, shape, cfg, allow)
            .await
            .map_err(unreadable)?;
        let Some(entry) = entry else {
            return Err(no_shape(key));
        };
        let found = join_status(cross, vec![entry], allow).await?;
        let mut one = render(&found[0]);
        one["previous_keys"] = Value::Null;
        return Ok(one);
    }
    let cfg = q.get("cfg");
    let start = position.as_deref().map(decode_position).transpose()?;
    let (entries, next) = across(
        &instances,
        start,
        size,
        |instance, after, room| async move {
            let page = reads
                .shape_cfgs(&instance, shape, after.as_ref(), room, allow)
                .await?;
            let keep = |e: &ShapeEntry| cfg.is_none_or(|c| e.cfg == c);
            Ok::<_, ReadError>((page.items.into_iter().filter(keep).collect(), page.next))
        },
    )
    .await?;
    if entries.is_empty() && next.is_none() && position.is_none() {
        return Err(no_shape(key));
    }
    let found = join_status(cross, entries, allow).await?;
    let mut answer = json!({"shapes": found.iter().map(render).collect::<Vec<_>>()});
    if let Some(next) = next {
        answer["next_page"] = q
            .next_page(&format!("/v1/shapes/{key}"), size, &q.token(ROUTE, &next))
            .into();
    }
    if partial {
        answer["partial"] = true.into();
    }
    Ok(answer)
}

/// `GET /v1/shapes/{key}/runs`: the last runs a test's status row keeps.
async fn ring(cross: &Cross, key: &str, parts: &Parts) -> Result<Value, Response> {
    const ROUTE: &str = "/v1/shapes/{key}/runs";
    let (readers, caller) = cross.begin(parts, ROUTE).await?;
    let shape = parse_key(key)?;
    let q = Query::parse(parts.uri.query().unwrap_or_default(), &["instance", "cfg"])?;
    let (Some(instance), Some(cfg)) = (q.get("instance"), q.get("cfg")) else {
        return Err(bad(
            "bad_request",
            "instance and cfg are required: a ring of runs belongs to one (instance, cfg)",
        ));
    };
    let mut scope = Scope::new(readers, &caller);
    let (instances, _) = cross.reach(&mut scope, &q).await?;
    let reach: BTreeSet<&str> = instances.iter().map(String::as_str).collect();
    let allow: Allow<'_> = &|name: &str| reach.contains(name);
    let reads = &cross.durable.reads;
    let (catalog, status) = tokio::try_join!(
        reads.shape(instance, shape, cfg, allow),
        reads.status(instance, shape, cfg, allow),
    )
    .map_err(unreadable)?;
    if catalog.is_none() && status.is_none() {
        return Err(no_shape(key));
    }
    let ring: Vec<Value> = status.as_ref().map_or_else(Vec::new, |s| {
        s.row.ring.iter().rev().map(ring_run).collect()
    });
    Ok(json!({
        "shape": key,
        "instance": instance,
        "cfg": cfg,
        "runs": ring,
        "complete": false,
        "covers": if status.is_some() { "recent_16" } else { "none" },
        "needs": "day_index",
    }))
}

/// One run of a ring.
fn ring_run(r: &RingRun) -> Value {
    let id = hex::encode(&r.run_id);
    let mut run = json!({
        "run_id": id,
        "run_href": format!("/v1/runs/{id}"),
        "class": class(r.status()),
        "execution_ms": r.execution_ms,
        "source": "run",
    });
    if r.digest16 != 0 {
        run["digest16"] = format!("{:016x}", r.digest16).into();
    }
    run
}

/// The key `text` is: 16 lowercase hex digits.
fn parse_key(text: &str) -> Result<u64, Response> {
    let valid = text.len() == 16 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    valid
        .then(|| u64::from_str_radix(text, 16).ok())
        .flatten()
        .ok_or_else(|| {
            bad(
                "bad_request",
                &format!("{text:?} is not a shape key: 16 lowercase hex digits"),
            )
        })
}

fn no_shape(key: &str) -> Response {
    missing(
        "no_shape",
        &format!("no shape {key} is known to an instance you may read"),
    )
}

fn parse_cursor(text: &str) -> Result<PageCursor, Response> {
    text.parse()
        .map_err(|_| bad("bad_page_token", "the page token holds no position"))
}

/// An instance and a position in its rows, which a page token carries as
/// `<length of the name>:<name><cursor>` (no cursor: the instance's first row).
type Position = (String, Option<PageCursor>);

fn encode_position((name, cursor): &Position) -> String {
    let cursor = cursor.as_ref().map(ToString::to_string).unwrap_or_default();
    format!("{}:{name}{cursor}", name.len())
}

fn decode_position(text: &str) -> Result<Position, Response> {
    let malformed = || bad("bad_page_token", "the page token holds no position");
    let (len, rest) = text.split_once(':').ok_or_else(malformed)?;
    let len: usize = len.parse().map_err(|_| malformed())?;
    let (name, cursor) = rest.get(..len).zip(rest.get(len..)).ok_or_else(malformed)?;
    let cursor = (!cursor.is_empty())
        .then(|| parse_cursor(cursor))
        .transpose()?;
    Ok((name.to_owned(), cursor))
}

/// Walks `instances` (sorted) from `start`, calling `fetch` (an instance, where in its
/// rows to start, and how many rows are wanted) until `limit` items are kept, the
/// instances end, or [`MAX_FETCHES`] pages were read. The position to resume from is
/// returned as the text a page token carries; a filter that keeps little makes a short
/// page with one.
async fn across<T, F, Fut>(
    instances: &[String],
    start: Option<Position>,
    limit: usize,
    fetch: F,
) -> Result<(Vec<T>, Option<String>), Response>
where
    F: Fn(String, Option<PageCursor>, usize) -> Fut,
    Fut: Future<Output = Result<(Vec<T>, Option<PageCursor>), ReadError>>,
{
    let (from, mut cursor) = start.map_or((None, None), |(name, c)| (Some(name), c));
    let mut at = from.map_or(0, |name| {
        let first = instances.iter().position(|i| *i >= name);
        // The cursor is a position in the instance it was made for.
        if first.is_none_or(|i| instances[i] != name) {
            cursor = None;
        }
        first.unwrap_or(instances.len())
    });
    let mut out = Vec::new();
    for fetched in 0.. {
        let Some(instance) = instances.get(at) else {
            break;
        };
        if out.len() >= limit || fetched >= MAX_FETCHES {
            return Ok((out, Some(encode_position(&(instance.clone(), cursor)))));
        }
        let room = limit - out.len();
        let (items, next) = fetch(instance.clone(), cursor.take(), room)
            .await
            .map_err(unreadable)?;
        out.extend(items);
        if next.is_some() {
            cursor = next;
        } else {
            at += 1;
        }
    }
    Ok((out, None))
}

/// The entries with the status rows of the tests among them.
async fn join_status(
    cross: &Cross,
    entries: Vec<ShapeEntry>,
    allow: Allow<'_>,
) -> Result<Vec<Found>, Response> {
    let reads = &cross.durable.reads;
    futures::future::try_join_all(entries.into_iter().map(|e| async move {
        let status = if e.row.kind() == ShapeKind::Test {
            reads.status(&e.instance, e.shape, &e.cfg, allow).await?
        } else {
            None
        };
        Ok::<_, ReadError>(Found {
            instance: e.instance,
            shape: e.shape,
            cfg: e.cfg,
            catalog: Some(e.row),
            status: status.map(|s| s.row),
        })
    }))
    .await
    .map_err(unreadable)
}

/// One row of an answer.
fn render(f: &Found) -> Value {
    let key = format!("{:016x}", f.shape);
    let test = f.status.is_some()
        || f.catalog
            .as_ref()
            .is_some_and(|c| c.kind() == ShapeKind::Test);
    let mut link = form_urlencoded::Serializer::new(String::new());
    link.append_pair("instance", &f.instance);
    link.append_pair("cfg", &f.cfg);
    let link = link.finish();
    let mut o = Map::new();
    o.insert("instance".into(), f.instance.clone().into());
    o.insert("key".into(), key.clone().into());
    o.insert("cfg".into(), f.cfg.clone().into());
    o.insert("kind".into(), if test { "test" } else { "other" }.into());
    o.insert("href".into(), format!("/v1/shapes/{key}?{link}").into());
    o.insert(
        "runs_href".into(),
        format!("/v1/shapes/{key}/runs?{link}").into(),
    );
    let target = f.catalog.as_ref().map(|c| c.target.as_str());
    o.insert(
        "target".into(),
        target
            .filter(|t| !t.is_empty())
            .map_or(Value::Null, Value::from),
    );
    if let Some(c) = &f.catalog {
        o.insert("first_seen".into(), at_ms(c.first_seen_ms));
    }
    if let Some(s) = &f.status {
        o.insert(
            "status".into(),
            s.kind().map_or(Value::Null, |k| k.name().into()),
        );
        o.insert(
            "flaky".into(),
            json!({
                "proven": s.proven,
                "proof_digest": s.proven.then(|| format!("{:016x}", s.proof_digest)),
                "since": s.proven_since.as_ref().map(mark),
            }),
        );
        for (name, m) in [("last_pass", &s.last_pass), ("first_fail", &s.first_fail)] {
            o.insert(name.into(), m.as_ref().map_or(Value::Null, mark));
        }
    }
    // The later of the two rows' times; a row of an answer has at least one of the two.
    let last_seen = f
        .catalog
        .iter()
        .map(|c| c.last_seen_ms)
        .chain(f.status.iter().map(|s| s.last_seen_ms))
        .max()
        .unwrap_or_default();
    o.insert("last_seen".into(), at_ms(last_seen));
    Value::Object(o)
}

/// A run a status row names.
fn mark(m: &Mark) -> Value {
    let id = hex::encode(&m.run_id);
    json!({"at": at_ms(m.time_ms), "run_id": id, "run_href": format!("/v1/runs/{id}")})
}

/// A ring run's class, by name.
fn class(status: TestStatus) -> &'static str {
    match status {
        TestStatus::Unspecified => "unspecified",
        TestStatus::Pass => "pass",
        TestStatus::Fail => "fail",
        TestStatus::Timeout => "timeout",
        TestStatus::OomKilled => "oom_killed",
        TestStatus::Infra => "infra",
    }
}

#[cfg(test)]
#[path = "shapes_tests.rs"]
mod tests;
