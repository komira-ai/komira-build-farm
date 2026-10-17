//! `GET /v1/grid`: the outcome of chosen shapes in chosen builds, one cell for each
//! (row, build), read from the run records of the builds' chunks. Nothing is folded:
//! the answer is what the named builds hold.
//!
//! A row is one `(instance, shape key, configuration)` that has a run in some column; a
//! build with no run of the row has the cell `absent`. A cell is the outcome of the row's
//! last run in the build that is a result (a pass, a failure, a timeout, a kill at the
//! action's own memory limit, or an infrastructure failure; a cancelled run or a kill the
//! ladder followed is no result). Not here yet: commit columns (they need the client
//! report), `cached_pass` cells (they need the hits of test shapes in windows) and run
//! length encoding.

#![allow(
    clippy::result_large_err,
    reason = "the Err is the answer to send, built once on a refusal; boxing it would only \
              move it"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::State;
use axum::http::request::Parts;
use axum::response::Response;
use kbf_history::records::{RunRecord, TestStatus};
use kbf_history::status::classify;
use kbf_history::{Allow, InvKey};
use serde_json::{Value, json};

use super::list::{Listing, Merged, merged};
use super::shapes::parse_key;
use super::{Cross, reply};
use crate::http_history::query::{Query, at_ms, bad, missing};
use crate::http_history::scope::Scope;
use crate::http_history::unreadable;

const CALL: &str = "GET /v1/grid";

/// Builds (columns) and shapes one grid has at most.
const MAX_COLUMNS: usize = 50;
const MAX_SHAPES: usize = 50;

/// Records of one build a grid reads at most: past it the column says `complete: false`.
const COLUMN_RECORDS: usize = 20_000;

pub(super) async fn page(State(cross): State<Arc<Cross>>, parts: Parts) -> Response {
    reply(read(&cross, &parts).await)
}

/// One build of the grid, with the runs read from it.
struct Column {
    id: String,
    first_seen_ms: u64,
    complete: bool,
    runs: Vec<RunRecord>,
}

/// A row: the instance, the shape key and the configuration.
type RowKey = (String, u64, String);

/// What a row's cell in one build says.
struct Cell {
    status: &'static str,
    run_id: String,
}

async fn read(cross: &Cross, parts: &Parts) -> Result<Value, Response> {
    let (readers, caller) = cross.begin(parts, CALL).await?;
    let q = Query::parse(
        parts.uri.query().unwrap_or_default(),
        &["shapes", "columns", "invocations", "last", "instance"],
    )?;
    match q.get("columns") {
        None | Some("invocation") => {}
        Some("commit") => {
            return Err(bad(
                "needs_report",
                "columns=commit needs the commit a client reports for its build, which \
                 history does not hold yet",
            ));
        }
        Some(other) => {
            return Err(bad(
                "bad_request",
                &format!("columns={other:?}: invocation (commit needs the client report)"),
            ));
        }
    }
    let shapes = shapes(&q)?;
    let mut scope = Scope::new(readers, &caller);
    let reads = &cross.durable.reads;
    let ids: Vec<String> = match (q.get("invocations"), q.get("last")) {
        (Some(_), Some(_)) | (None, None) => {
            return Err(bad(
                "bad_request",
                "name the builds by invocations=<id>,<id> or by last=<n>, one of the two",
            ));
        }
        (Some(list), None) => ids(list)?,
        (None, Some(last)) => {
            let n = last
                .parse::<usize>()
                .ok()
                .filter(|n| (1..=MAX_COLUMNS).contains(n))
                .ok_or_else(|| {
                    bad(
                        "bad_request",
                        &format!("last={last:?}: 1 to {MAX_COLUMNS} builds"),
                    )
                })?;
            let (instances, _) = cross.reach(&mut scope, &q).await?;
            let allow: Allow<'_> = &|instance: &str| scope.allows(instance);
            let filter = Listing {
                since_ms: 0,
                until_ms: u64::MAX,
                state: None,
                size: n,
                after: None,
            };
            let Merged { items, .. } = merged(reads, &instances, &filter, allow).await?;
            items.into_iter().map(|i| i.header.record_id).collect()
        }
    };
    // Each build's instance names, then every one of them asked of the authorizer at once.
    let mut names = Vec::new();
    for id in &ids {
        let key = InvKey::from_record_id(id);
        names.push(reads.instances(&key).await.map_err(unreadable)?);
    }
    let asking: Vec<&str> = names
        .iter()
        .flatten()
        .flatten()
        .map(String::as_str)
        .collect();
    scope.ask(&asking).await;
    let allow: Allow<'_> = &|instance: &str| scope.allows(instance);
    let mut columns = Vec::new();
    for id in ids {
        let key = InvKey::from_record_id(&id);
        let held = reads.invocation(&key, allow).await.map_err(unreadable)?;
        let gathered = match held {
            Some(view) if view.newest.is_some() && view.header.record_id == id => {
                let got = cross
                    .durable
                    .gather_up_to(&key, allow, COLUMN_RECORDS)
                    .await
                    .map_err(unreadable)?;
                got.map(|g| (view, g))
            }
            _ => None,
        };
        // Not held, or held and not for this caller: the same answer for both.
        let Some((view, got)) = gathered else {
            return Err(missing(
                "no_record",
                &format!("no invocation {id:?} is held in history that you may read"),
            ));
        };
        columns.push(Column {
            id,
            first_seen_ms: view.header.first_seen_ms,
            complete: !got.capped,
            runs: got.runs,
        });
    }
    Ok(grid(&columns, &shapes))
}

/// The shape keys of `shapes=`: one to [`MAX_SHAPES`], each 16 lowercase hex digits.
fn shapes(q: &Query) -> Result<BTreeSet<u64>, Response> {
    let Some(list) = q.get("shapes") else {
        return Err(bad(
            "bad_request",
            "shapes is required: shape keys, comma separated",
        ));
    };
    let keys = list
        .split(',')
        .map(parse_key)
        .collect::<Result<BTreeSet<u64>, Response>>()?;
    if keys.len() > MAX_SHAPES {
        return Err(bad("bad_request", &format!("at most {MAX_SHAPES} shapes")));
    }
    Ok(keys)
}

/// The invocation ids of `invocations=`: one to [`MAX_COLUMNS`], none twice.
fn ids(list: &str) -> Result<Vec<String>, Response> {
    let ids: Vec<&str> = list.split(',').collect();
    let unique: BTreeSet<&str> = ids.iter().copied().collect();
    if ids.len() > MAX_COLUMNS || unique.len() != ids.len() || ids.iter().any(|i| i.is_empty()) {
        return Err(bad(
            "bad_request",
            &format!("invocations: 1 to {MAX_COLUMNS} different ids, comma separated"),
        ));
    }
    Ok(ids.into_iter().map(str::to_owned).collect())
}

/// The cell of `status`'s test result, in the grid's words.
fn word(status: TestStatus) -> &'static str {
    match status {
        TestStatus::Pass => "pass",
        TestStatus::Fail | TestStatus::Timeout => "fail",
        TestStatus::OomKilled => "oom_killed",
        TestStatus::Infra | TestStatus::Unspecified => "infra",
    }
}

/// The last result of each row of `shapes` in `runs`, by the time of the run and then its
/// id.
fn cells(runs: &[RunRecord], shapes: &BTreeSet<u64>) -> BTreeMap<RowKey, Cell> {
    let mut last: BTreeMap<RowKey, (&RunRecord, TestStatus)> = BTreeMap::new();
    for run in runs.iter().filter(|r| shapes.contains(&r.shape)) {
        let Some(status) = classify(run) else {
            continue;
        };
        let key = (run.instance.clone(), run.shape, run.cfg.clone());
        let newer = |held: &&RunRecord| (run.time_ms, run.run_id()) > (held.time_ms, held.run_id());
        if last.get(&key).is_none_or(|(held, _)| newer(held)) {
            last.insert(key, (run, status));
        }
    }
    last.into_iter()
        .map(|(key, (run, status))| {
            let cell = Cell {
                status: word(status),
                run_id: run.run_id().to_string(),
            };
            (key, cell)
        })
        .collect()
}

/// The grid of `columns`: a row for each key any column has, a cell for each column.
fn grid(columns: &[Column], shapes: &BTreeSet<u64>) -> Value {
    let by_column: Vec<BTreeMap<RowKey, Cell>> =
        columns.iter().map(|c| cells(&c.runs, shapes)).collect();
    let rows: BTreeSet<&RowKey> = by_column.iter().flat_map(BTreeMap::keys).collect();
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|key| {
            let (instance, shape, cfg) = key;
            let cells: Vec<Value> = by_column
                .iter()
                .map(|column| match column.get(key) {
                    Some(c) => json!({
                        "status": c.status,
                        "run_id": c.run_id,
                        "run_href": format!("/v1/runs/{}", c.run_id),
                    }),
                    None => json!({"status": "absent"}),
                })
                .collect();
            let key = format!("{shape:016x}");
            let mut link = form_urlencoded::Serializer::new(String::new());
            link.append_pair("instance", instance);
            link.append_pair("cfg", cfg);
            json!({
                "instance": instance,
                "key": key,
                "cfg": cfg,
                "href": format!("/v1/shapes/{key}?{}", link.finish()),
                "cells": cells,
            })
        })
        .collect();
    let columns: Vec<Value> = columns
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "href": format!("/v1/invocations/{}", c.id),
                "first_seen": at_ms(c.first_seen_ms),
                "complete": c.complete,
            })
        })
        .collect();
    json!({
        "columns": columns,
        "rows": rows,
        "shapes": shapes.iter().map(|s| format!("{s:016x}")).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
#[path = "grid_tests.rs"]
mod tests;
