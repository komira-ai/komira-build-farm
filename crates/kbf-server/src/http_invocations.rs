//! The build pages' read API on the operator API listener: `GET /v1/invocations/{id}`
//! and the routes below it, over the records the in-memory recorder keeps
//! (`docs/design/invocations.md` sections 4.1 to 4.5, `docs/api.md`).
//!
//! - **Off unless the policy names it.** The routes run the REAPI policy
//!   (`--reapi-auth-policy`): its authenticator on the request, then its
//!   `invocations.getAuthorizer` ([`kbf_auth::Policy::invocations_get`]) on every
//!   instance name the invocation's calls used, all of which must allow.
//!   `--invocations-open` fills in `allow` for it. Without either, every request is
//!   `404`, and so is every request to the blob route ([`crate::http_blobs`]).
//! - **Links to blobs** (`href`) are `null` when the blob route is off: the policy
//!   names no `blobs.getAuthorizer`.
//! - **The routes**: the invocation (its header fields, counters and links); a page of
//!   action rows (`?outcome=`, `?how=`); one action, with the outputs, stdout, stderr
//!   and usage copied into its row and whether each blob is still held; a page of
//!   outputs; a page of logs, failed actions first; the aggregates of section 4.4,
//!   computed from the rows at each request.
//! - **Pages** (section 4.5): `?page_size=` (100 by default, at most 1000) and the
//!   `next_page_token` of the page before. A token names the record it was made for,
//!   its filter and a position in rows that are only ever appended, so paging while a
//!   build writes neither skips nor repeats a row. A token used with another filter or
//!   on another route is `400`; one made for a record that is gone (removed at the
//!   memory bound, and perhaps made again under the same key since) is `404`.
//! - `{id}` is an id of section 2.2 or `<id>~<n>`, matched as the bytes of the path:
//!   anything else (`<`, `%3C`, `/`) is `404`.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use kbf_auth::{Authenticator, Authorizer, Policy, authorize};
use kbf_front::{Cache, MetaLog, Recorder, wall_now};
use kbf_invocation::WallTime;
use kbf_objstore::ObjectStore;
use serde_json::Value;

use crate::history::HistoryReads;
use crate::http_blobs::{error, off, refusal};
use crate::http_history::{Durable, run_router};

mod pages;
mod views;

pub(crate) use pages::{Bad, Request, answer};
pub use pages::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE};
pub(crate) use views::{Draft, rfc3339};

/// The call name refusals are logged with.
pub(crate) const CALL: &str = "GET /v1/invocations";

/// Who may read the build pages: the REAPI policy's authenticator and its
/// `invocations.getAuthorizer`, and whether the blob route answers.
#[derive(Clone)]
pub struct InvocationReaders {
    pub(crate) authenticator: Arc<dyn Authenticator>,
    pub(crate) authorizer: Arc<dyn Authorizer>,
    pub(crate) blobs: bool,
}

impl InvocationReaders {
    /// The readers `policy` allows, or `None` (the routes are off) when it names no
    /// `invocations.getAuthorizer`.
    #[must_use]
    pub fn from_policy(policy: &Policy) -> Option<Self> {
        let authorizer = Arc::clone(policy.invocations_get.as_ref()?);
        Some(Self {
            authenticator: Arc::clone(&policy.authenticator),
            authorizer,
            blobs: policy.blobs_get.is_some(),
        })
    }
}

struct PagesState<M, O> {
    recorder: Recorder,
    cache: Arc<Cache<M, O>>,
    readers: Option<InvocationReaders>,
    /// History to read a record from when the recorder does not hold it; `None` without
    /// `--history-objects`.
    durable: Option<Arc<Durable>>,
    /// When the routes were made, with the server: no record is older.
    started: WallTime,
}

/// The build pages over `recorder`'s records, saying whether each blob is still in
/// `cache`; with `readers` `None` they answer every request `404`. With `history`, a
/// record the recorder does not hold is read back from it, and `GET /v1/runs/{id}`
/// answers ([`crate::http_history`]); without it the routes are what they were.
pub fn router<M, O>(
    recorder: Recorder,
    cache: Arc<Cache<M, O>>,
    readers: Option<InvocationReaders>,
    history: Option<Arc<HistoryReads>>,
) -> Router
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let durable = history.map(|reads| Arc::new(Durable::new(reads)));
    let runs = durable
        .as_ref()
        .map(|d| run_router(recorder.clone(), readers.clone(), Arc::clone(d)));
    let state = Arc::new(PagesState {
        recorder,
        cache,
        readers,
        durable,
        started: wall_now(),
    });
    let pages = Router::new()
        .route("/v1/invocations/{id}", get(page::<M, O>))
        .route("/v1/invocations/{id}/{*rest}", get(page::<M, O>))
        .with_state(state);
    match runs {
        Some(runs) => pages.merge(runs),
        None => pages,
    }
}

/// The `404` of the build pages when the policy does not turn them on.
pub(crate) fn pages_off() -> Response {
    off(
        "invocations",
        "the build pages are off: the REAPI policy names no invocations.getAuthorizer \
         and --invocations-open is not given",
    )
}

async fn page<M, O>(State(state): State<Arc<PagesState<M, O>>>, parts: Parts) -> Response
where
    M: MetaLog,
    O: ObjectStore + 'static,
{
    let Some(readers) = &state.readers else {
        return pages_off();
    };
    // The later of the server's start and the newest client record removed at the
    // bound: a record last seen after it would still be held.
    let since = state
        .recorder
        .evicted_last_seen()
        .map_or(state.started, |t| t.max(state.started));
    let draft = match read(
        readers,
        &state.recorder,
        state.durable.as_deref(),
        &parts,
        since,
    )
    .await
    {
        Ok(draft) => draft,
        Err(answer) => return answer,
    };
    let digests: Vec<_> = draft.blobs.iter().map(|(_, d)| *d).collect();
    // An index that does not answer leaves every `available` null: the page is still
    // what the record holds.
    let present = state.cache.present(&digests).await.unwrap_or_default();
    json(&draft.fill(&present))
}

/// The answer to one request before its blobs are looked up: the caller
/// authenticated and allowed on every instance name of the record, then the route
/// read from it.
async fn read(
    readers: &InvocationReaders,
    recorder: &Recorder,
    durable: Option<&Durable>,
    parts: &Parts,
    since: WallTime,
) -> Result<views::Draft, Response> {
    let caller = readers
        .authenticator
        .authenticate(parts)
        .await
        .map_err(|e| {
            tracing::warn!(call = CALL, error = %e.message(), "invocation read not authenticated");
            refusal(&e)
        })?;
    let request = Request::parse(parts.uri.path(), parts.uri.query().unwrap_or_default())
        .map_err(|bad| bad_answer(&bad, "", since))?;
    let id = request.key.to_string();
    let mut held = None;
    recorder.read(&request.key, &mut |inv| {
        held = Some((inv.serial(), pages::instance_names(inv)));
    });
    let Some((serial, names)) = held else {
        // Not held: read back from history when there is any.
        return match durable {
            Some(durable) => {
                durable
                    .read(readers, &caller, &request, since, wall_now())
                    .await
            }
            None => Err(bad_answer(&Bad::Gone, &id, since)),
        };
    };
    // Every name must be allowed, each asked as a REAPI call's instance name is (a
    // refusal is logged there; no answer is a refusal too).
    for name in &names {
        authorize(&*readers.authorizer, &caller, CALL, name)
            .await
            .map_err(|e| refusal(&e))?;
    }
    let now = wall_now();
    // Removed since it was authorized: as if it had never been held.
    let mut built = Err(Bad::Gone);
    recorder.read(&request.key, &mut |inv| {
        built = pages::answer(inv, &request, (serial, &names), readers.blobs, now);
    });
    built.map_err(|bad| bad_answer(&bad, &id, since))
}

/// The answer to a request [`pages`] refused; a record that is gone answers with
/// `{"error": "no_record", "id": <id>, "records_since": <since>}`
/// (`docs/design/invocations-api.md` 4.10), which no other `404` has.
pub(crate) fn bad_answer(bad: &Bad, id: &str, since: WallTime) -> Response {
    match bad {
        Bad::Gone => {
            let body = serde_json::json!({
                "error": "no_record",
                "id": id,
                "records_since": views::rfc3339(since),
            });
            (
                StatusCode::NOT_FOUND,
                [(axum::http::header::CONTENT_TYPE, crate::api::JSON)],
                body.to_string(),
            )
                .into_response()
        }
        Bad::NotFound(why) => error(StatusCode::NOT_FOUND, why),
        Bad::Invalid(why) => error(StatusCode::BAD_REQUEST, why),
        Bad::Changed(why) => error(StatusCode::SERVICE_UNAVAILABLE, why),
    }
}

/// `value` as a JSON body.
pub(crate) fn json(value: &Value) -> Response {
    let body = value.to_string();
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, crate::api::JSON)],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests;
