//! `GET /v1/runs/{run_id}/logs/{stdout|stderr|result}`: the copy of a failed run's log
//! that history kept (`docs/design/ui-data.md` 5.3 and Q21), served after the CAS blob it
//! was copied from is gone.
//!
//! - **Scoped to the run's instance, in two phases.** The run is found first (the recorder,
//!   or history) and its one instance authorized as a REAPI call's instance name is; only
//!   then is the copy read, with an `allow` that accepts that instance and no other, and
//!   the reader refuses an object that names another. A caller refused learns nothing: the
//!   refusal comes before any read of the copy.
//! - **Raw bytes, never a page.** As the blob route: `application/octet-stream`,
//!   `Content-Disposition: attachment`, `X-Content-Type-Options: nosniff`,
//!   `Content-Security-Policy: sandbox`, on every answer, errors included.
//! - **One byte range.** `Range: bytes=...` with a single range is answered `206`, one
//!   that cannot be satisfied `416`, anything else is ignored and the whole log is sent
//!   (RFC 9110, as the blob route does). Only the blocks that hold the range are read.
//! - **A memory budget.** The bytes of the range are held in memory until sent, and count
//!   against the process's read budget, the one the blob route and the history writer's
//!   tail reads share; a read waits for its share at most [`READ_BUDGET_WAIT`], then is
//!   `503` with `Retry-After`.
//! - **No parameters.** A query is refused `400`: nothing on this route is chosen by one.

use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_RANGE, CONTENT_SECURITY_POLICY, CONTENT_TYPE,
    RANGE, RETRY_AFTER, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use kbf_auth::authorize;
use kbf_history::RunId;
use kbf_history::records::FlogStream;

use super::run::{PREFIX, RunState, find, no_run, parse};
use super::unreadable;
use crate::history::HistoryReads;
use crate::http_blobs::{
    Held, READ_BUDGET_WAIT, ReadBudget, Refused, Wanted, error, header, refusal, wanted,
};
use crate::http_invocations::pages_off;

/// The call name refusals are logged with.
const CALL: &str = "GET /v1/runs/logs";

/// The `Retry-After` of the `503` of a read whose share of the budget was not free, in
/// seconds.
const RETRY_AFTER_SECONDS: &str = "1";

pub(super) async fn page(State(state): State<Arc<RunState>>, parts: Parts) -> Response {
    let mut answer = match answer(&state, &parts).await {
        Ok(served) | Err(served) => served,
    };
    let headers = answer.headers_mut();
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox"));
    answer
}

/// What the path names: a run and one of its logs; or why it names none.
fn parse_path(path: &str) -> Result<(RunId, FlogStream), String> {
    let rest = path.strip_prefix(PREFIX).unwrap_or_default();
    let mut parts = rest.split('/');
    let (id, logs, stream, more) = (parts.next(), parts.next(), parts.next(), parts.next());
    let Some(id) = id.and_then(parse) else {
        return Err(format!(
            "{rest:?} does not name a run: 24 lowercase hex digits, then /logs/<log>"
        ));
    };
    match (logs, stream.and_then(FlogStream::named), more) {
        (Some("logs"), Some(stream), None) => Ok((id, stream)),
        _ => Err(format!(
            "{rest:?} is not a log of a run: the logs are stdout, stderr and result"
        )),
    }
}

async fn answer(state: &RunState, parts: &Parts) -> Result<Response, Response> {
    let Some(readers) = &state.readers else {
        return Err(pages_off());
    };
    let caller = readers
        .authenticator
        .authenticate(parts)
        .await
        .map_err(|e| {
            tracing::warn!(call = CALL, error = %e.message(), "log read not authenticated");
            refusal(&e)
        })?;
    let (id, stream) =
        parse_path(parts.uri.path()).map_err(|why| error(StatusCode::NOT_FOUND, &why))?;
    if parts.uri.query().is_some_and(|q| !q.is_empty()) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "this route takes no parameters; the range is the Range header",
        ));
    }
    // Phase 1: the run, and its one instance, authorized.
    let Some((record, _)) = find(state, id).await? else {
        return Err(no_run(&id.to_string()));
    };
    authorize(&*readers.authorizer, &caller, CALL, &record.instance)
        .await
        .map_err(|e| refusal(&e))?;
    // Phase 2: the copy, read for that instance only.
    serve(
        &state.durable.reads,
        &state.budget,
        (id, stream),
        &record.instance,
        parts.headers.get(RANGE),
    )
    .await
}

/// The copy of `log` of run `id` for `instance`, as the `Range` header asks, read under
/// `budget`. `instance` is the one the caller was authorized on: the copy is read for it
/// and no other.
async fn serve(
    reads: &HistoryReads,
    budget: &ReadBudget,
    (id, stream): (RunId, FlogStream),
    instance: &str,
    range: Option<&HeaderValue>,
) -> Result<Response, Response> {
    let allow = |i: &str| i == instance;
    let log = reads
        .failure_log(id, stream, &allow)
        .await
        .map_err(unreadable)?;
    let Some(log) = log else {
        let body = serde_json::json!({
            "error": "no_log", "id": id.to_string(), "log": stream.name(),
        });
        let answer = (
            StatusCode::NOT_FOUND,
            [(CONTENT_TYPE, crate::api::JSON)],
            body.to_string(),
        );
        return Err(answer.into_response());
    };
    let len = log.len();
    let mut headers = HeaderMap::new();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(CONTENT_DISPOSITION, HeaderValue::from_static("attachment"));
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    let (code, first, last) = match wanted(range, len) {
        Wanted::Whole => (StatusCode::OK, 0, len.saturating_sub(1)),
        Wanted::Part { first, last } => {
            headers.insert(CONTENT_RANGE, header(format!("bytes {first}-{last}/{len}")));
            (StatusCode::PARTIAL_CONTENT, first, last)
        }
        Wanted::Unsatisfiable => {
            headers.insert(CONTENT_RANGE, header(format!("bytes */{len}")));
            return Ok((StatusCode::RANGE_NOT_SATISFIABLE, headers, Body::empty()).into_response());
        }
    };
    // An empty log has nothing to read and nothing to hold the budget for.
    if len == 0 {
        return Ok((code, headers, Body::empty()).into_response());
    }
    let wanted = last - first + 1;
    let permit = budget
        .acquire(wanted, READ_BUDGET_WAIT)
        .await
        .map_err(|refused| busy(refused, wanted))?;
    let bytes = reads
        .read_log(&log, first, last)
        .await
        .map_err(unreadable)?;
    let bytes = Bytes::from_owner(Held {
        bytes,
        _permit: permit,
    });
    Ok((code, headers, Body::from(bytes)).into_response())
}

/// The `503` of a read whose share of the budget was not free in time (`Retry-After` and
/// `"busy": true`), or that could never be: more bytes than the whole budget.
fn busy(refused: Refused, bytes: u64) -> Response {
    let (why, busy) = match refused {
        Refused::Busy => (
            format!(
                "the reads in progress hold the memory this read of {bytes} bytes needs, and \
                 it was not free within {} s; try again",
                READ_BUDGET_WAIT.as_secs()
            ),
            true,
        ),
        Refused::TooLarge => (
            format!("this read of {bytes} bytes is more than the memory reads may hold"),
            false,
        ),
    };
    let body = serde_json::json!({ "error": why, "busy": busy }).to_string();
    let headers = [
        (CONTENT_TYPE, HeaderValue::from_static(crate::api::JSON)),
        (RETRY_AFTER, HeaderValue::from_static(RETRY_AFTER_SECONDS)),
    ];
    (StatusCode::SERVICE_UNAVAILABLE, headers, body).into_response()
}

#[cfg(test)]
#[path = "logs_tests.rs"]
mod tests;
