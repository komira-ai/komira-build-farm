//! The routes that read across builds (`docs/design/ui-data.md` sections 4.3, 5.5, 6.2
//! and 8): the invocation list, the shape catalog and the tests, answered from the
//! index rows the history writer keeps and never from a chunk.
//!
//! - **Off unless history is on and the policy names the readers**, as the build pages
//!   are: no `--history-objects` leaves none of these routes, and no
//!   `invocations.getAuthorizer` answers every request `404`.
//! - **Scoped by instance on every route.** The authorizer is asked once per request
//!   about each distinct instance the request would touch ([`Scope`]); a list holds only
//!   rows of allowed instances, and an invocation only when every instance it used is
//!   allowed. An instance named by the query that the caller may not read is `403`.
//! - **Paged**: `page_size` (1 to 200, 100 by default) and the `next_page` link of the
//!   page before. A token names its route and its filters and the position after the
//!   last row; it stays valid across restarts, and rows that arrive newer than a list's
//!   token do not move its pages.
//! - **Honest about what is not kept**: a filter or status that needs data history does
//!   not hold yet answers `400` with a name (`needs_report`, `unsupported_filter`,
//!   `unsupported_status`), never an empty page.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::request::Parts;
use axum::response::Response;
use axum::routing::get;
use kbf_auth::AuthenticationMetadata;
use serde_json::Value;

use super::query::Query;
use super::scope::Scope;
use super::{Durable, unreadable};
use crate::http_blobs::refusal;
use crate::http_invocations::{InvocationReaders, json, pages_off};

mod list;
mod shapes;

/// How long the instance names read from the index are used again.
pub(super) const NAMES_FRESH: Duration = Duration::from_secs(60);

/// The instances that have invocation lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Names {
    pub(super) list: Vec<String>,
    /// More instances exist than were read.
    pub(super) truncated: bool,
}

/// [`Names`], held for [`NAMES_FRESH`].
#[derive(Default)]
pub(super) struct NameCache {
    held: Option<(Instant, Names)>,
}

impl NameCache {
    /// The names read less than [`NAMES_FRESH`] before `now`.
    pub(super) fn get(&self, now: Instant) -> Option<Names> {
        let (at, names) = self.held.as_ref()?;
        (now.saturating_duration_since(*at) < NAMES_FRESH).then(|| names.clone())
    }

    /// Holds `names`, read at `now`.
    pub(super) fn put(&mut self, now: Instant, names: Names) {
        self.held = Some((now, names));
    }
}

/// What the routes share.
pub(super) struct Cross {
    readers: Option<InvocationReaders>,
    pub(super) durable: Arc<Durable>,
    names: Mutex<NameCache>,
}

impl Cross {
    /// The caller, authenticated; or the answer to give instead.
    pub(super) async fn begin(
        &self,
        parts: &Parts,
        call: &'static str,
    ) -> Result<(&InvocationReaders, Arc<AuthenticationMetadata>), Response> {
        let Some(readers) = &self.readers else {
            return Err(pages_off());
        };
        let caller = readers
            .authenticator
            .authenticate(parts)
            .await
            .map_err(|e| {
                tracing::warn!(call, error = %e.message(), "history read not authenticated");
                refusal(&e)
            })?;
        Ok((readers, caller))
    }

    /// The instances a request lists: the one `instance=` names, which must be allowed
    /// (`403` otherwise), or every instance that has lists and is allowed; and whether
    /// that is not all of them (more instances exist than the index reads at once).
    ///
    /// Every instance that has lists is put to the authorizer either way, not only the
    /// ones listed: an invocation shows only when all the instances it used are allowed,
    /// and `scope` must know the answer for each of them.
    pub(super) async fn reach(
        &self,
        scope: &mut Scope<'_>,
        q: &Query,
    ) -> Result<(Vec<String>, bool), Response> {
        let names = self.names().await?;
        let named = q.get("instance");
        let asking: Vec<&str> = names.list.iter().map(String::as_str).chain(named).collect();
        scope.ask(&asking).await;
        if let Some(name) = named {
            if let Some(why) = scope.refusal(name) {
                return Err(refusal(why));
            }
            return Ok((vec![name.to_owned()], false));
        }
        let mut allowed: Vec<String> = scope
            .allowed(&names.list)
            .into_iter()
            .map(str::to_owned)
            .collect();
        allowed.sort();
        Ok((allowed, names.truncated))
    }

    fn cache(&self) -> MutexGuard<'_, NameCache> {
        // One optional value; a panic elsewhere leaves it whole.
        self.names.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every instance that has invocation lists, from the cache when it is fresh.
    pub(super) async fn names(&self) -> Result<Names, Response> {
        if let Some(names) = self.cache().get(Instant::now()) {
            return Ok(names);
        }
        let read = self
            .durable
            .reads
            .instance_names()
            .await
            .map_err(unreadable)?;
        let names = Names {
            list: read.names,
            truncated: read.truncated,
        };
        self.cache().put(Instant::now(), names.clone());
        Ok(names)
    }
}

/// `answer` as the response: its JSON, or the refusal.
pub(super) fn reply(answer: Result<Value, Response>) -> Response {
    match answer {
        Ok(value) => json(&value),
        Err(response) => response,
    }
}

/// The cross-build routes over `durable`'s history.
pub(crate) fn router(readers: Option<InvocationReaders>, durable: Arc<Durable>) -> Router {
    let state = Arc::new(Cross {
        readers,
        durable,
        names: Mutex::new(NameCache::default()),
    });
    Router::new()
        .route("/v1/invocations", get(list::page))
        .route("/v1/shapes", get(shapes::shapes))
        .route("/v1/tests", get(shapes::tests))
        .route("/v1/shapes/{key}", get(shapes::one))
        .route("/v1/shapes/{key}/runs", get(shapes::runs))
        .with_state(state)
}

#[cfg(test)]
#[path = "cross_tests.rs"]
mod tests;
