//! The query strings, page tokens and error answers of the cross-build routes.

#![allow(
    clippy::result_large_err,
    reason = "the Err is the answer to send, built once on a refusal; boxing it would only \
              move it"
)]

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use kbf_history::MAX_PAGE;
use kbf_history::records::InvState;
use kbf_invocation::WallTime;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::http_invocations::rfc3339;

/// Rows a page holds when `page_size` is not given.
pub(super) const DEFAULT_PAGE: usize = 100;

/// A time in milliseconds after the epoch, as the API writes one.
pub(super) fn at_ms(ms: u64) -> Value {
    rfc3339(WallTime::from_unix_micros(ms.saturating_mul(1_000))).into()
}

/// A `400` that says what is wrong by a name a client can match: `{"error": <code>,
/// "detail": <text>}`.
pub(super) fn bad(code: &str, detail: &str) -> Response {
    let body = json!({"error": code, "detail": detail}).to_string();
    (
        StatusCode::BAD_REQUEST,
        [(axum::http::header::CONTENT_TYPE, crate::api::JSON)],
        body,
    )
        .into_response()
}

/// A `404` of the shape `{"error": <code>, "detail": <text>}`.
pub(super) fn missing(code: &str, detail: &str) -> Response {
    let body = json!({"error": code, "detail": detail}).to_string();
    (
        StatusCode::NOT_FOUND,
        [(axum::http::header::CONTENT_TYPE, crate::api::JSON)],
        body,
    )
        .into_response()
}

/// A parsed query string: each name at most once, only names the route takes.
pub(super) struct Query {
    pairs: Vec<(String, String)>,
}

impl Query {
    /// The pairs of `text` (after the `?`), which must all be named in `known`.
    ///
    /// # Errors
    /// A `400` for a name the route does not take, or one given twice.
    pub(super) fn parse(text: &str, known: &[&str]) -> Result<Query, Response> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (name, value) in form_urlencoded::parse(text.as_bytes()) {
            if !known.contains(&&*name) {
                return Err(bad(
                    "bad_request",
                    &format!("{name:?} is not a parameter of this route; it takes {known:?}"),
                ));
            }
            if pairs.iter().any(|(n, _)| *n == name) {
                return Err(bad(
                    "bad_request",
                    &format!("{name:?} is given more than once"),
                ));
            }
            pairs.push((name.into_owned(), value.into_owned()));
        }
        Ok(Query { pairs })
    }

    /// The value of `name`, if it was given.
    pub(super) fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The page size: 1 to [`MAX_PAGE`], [`DEFAULT_PAGE`] when not given.
    ///
    /// # Errors
    /// A `400` for anything else.
    pub(super) fn page_size(&self) -> Result<usize, Response> {
        let Some(text) = self.get("page_size") else {
            return Ok(DEFAULT_PAGE);
        };
        text.parse()
            .ok()
            .filter(|n| (1..=MAX_PAGE).contains(n))
            .ok_or_else(|| {
                bad(
                    "bad_request",
                    &format!("page_size={text:?}: a page holds 1 to {MAX_PAGE}"),
                )
            })
    }

    /// The filters as text, which a page token carries so that a token used with other
    /// filters is refused: every pair but the page's own, by name.
    pub(super) fn filter_text(&self) -> String {
        let mut out = form_urlencoded::Serializer::new(String::new());
        self.append_filters(&mut out);
        out.finish()
    }

    fn append_filters(&self, out: &mut form_urlencoded::Serializer<'_, String>) {
        let mut filters: Vec<&(String, String)> = self
            .pairs
            .iter()
            .filter(|(n, _)| n != "page_size" && n != "page_token")
            .collect();
        filters.sort();
        for (name, value) in filters {
            out.append_pair(name, value);
        }
    }

    /// The link to the page after this one: the route, the filters, the page size and
    /// the token.
    pub(super) fn next_page(&self, route: &str, size: usize, token: &str) -> String {
        let mut out = form_urlencoded::Serializer::new(String::new());
        self.append_filters(&mut out);
        out.append_pair("page_size", &size.to_string());
        out.append_pair("page_token", token);
        format!("{route}?{}", out.finish())
    }

    /// The position the request's `page_token` names, for `route` and these filters.
    ///
    /// # Errors
    /// A `400` for a token that does not decode or was made for another route or other
    /// filters.
    pub(super) fn position(&self, route: &str) -> Result<Option<String>, Response> {
        let Some(text) = self.get("page_token") else {
            return Ok(None);
        };
        let malformed = || bad("bad_page_token", &format!("{text:?} is not a page token"));
        let decoded = hex::decode(text)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(malformed)?;
        let mut fields = decoded.splitn(3, '\n');
        let (Some(for_route), Some(filter), Some(position)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err(malformed());
        };
        if for_route != route {
            return Err(bad(
                "bad_page_token",
                &format!("this page token was made for {for_route}, not {route}"),
            ));
        }
        if filter != self.filter_text() {
            return Err(bad(
                "bad_page_token",
                "this page token was made for other filters: send those of the first page",
            ));
        }
        Ok(Some(position.to_owned()))
    }

    /// The token for the page that starts at `position`.
    pub(super) fn token(&self, route: &str, position: &str) -> String {
        hex::encode(format!("{route}\n{}\n{position}", self.filter_text()))
    }

    /// A time of the query, RFC 3339, in milliseconds since the epoch.
    ///
    /// # Errors
    /// A `400` for text that is not an RFC 3339 time at or after the epoch.
    pub(super) fn time_ms(&self, name: &str) -> Result<Option<u64>, Response> {
        let Some(text) = self.get(name) else {
            return Ok(None);
        };
        OffsetDateTime::parse(text, &Rfc3339)
            .ok()
            .and_then(|t| u64::try_from(t.unix_timestamp_nanos() / 1_000_000).ok())
            .map(Some)
            .ok_or_else(|| {
                bad(
                    "bad_request",
                    &format!("{name}={text:?}: an RFC 3339 time, such as 2026-10-11T09:00:00Z"),
                )
            })
    }

    /// The `state` of the query: `open` or `closed`.
    ///
    /// # Errors
    /// A `400` for another word.
    pub(super) fn state(&self) -> Result<Option<InvState>, Response> {
        match self.get("state") {
            None => Ok(None),
            Some("open") => Ok(Some(InvState::Open)),
            Some("closed") => Ok(Some(InvState::Closed)),
            Some(other) => Err(bad(
                "bad_request",
                &format!("state={other:?}: open or closed"),
            )),
        }
    }
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;
