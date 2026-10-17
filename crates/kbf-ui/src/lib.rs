//! `kbf-ui`: the farm's server-rendered pages (`docs/design/ui-data.md` 6.1 and 8,
//! "UI pages").
//!
//! - **Over the read API only.** A page reads the same JSON documents the `/v1` routes
//!   answer, through a [`Source`] the server gives it, and parses them into the shapes
//!   of [`api`], never the server's structs. Each page declares the routes it reads
//!   ([`read::Page`]); its reader refuses any other URL before sending it, and every
//!   page lists the URLs it read, each a link an agent can follow for the JSON.
//! - **Escaped by construction.** Markup is written only through [`html::Html`],
//!   which escapes every string and has no public raw write.
//! - **Hardened answers.** Every [`Answer`], pages, the stylesheet and errors alike,
//!   carries [`CONTENT_SECURITY_POLICY`] and `X-Content-Type-Options: nosniff`.
//! - **No client toolchain.** One embedded stylesheet, no script, no CDN. Colours and
//!   spacing are CSS custom properties, light and dark by `prefers-color-scheme`, so a
//!   new look is a new stylesheet.
//!
//! The pages, under [`PREFIX`]: `/ui` (Builds: look a build up by id; with `?id=`, its
//! summary from `GET /v1/invocations/{id}`), `/ui/builds/{id}` (Build: the summary
//! and a page of `GET /v1/invocations/{id}/actions`), and
//! `/ui/builds/{id}/actions/{digest}` (Action: `GET /v1/invocations/{id}/actions/
//! {digest}`, with stdout and stderr shown as text when the blob route links them
//! and they are small; a larger one is a link).
//!
//! Not mounted on a server yet. A mount routes `GET` under [`PREFIX`] to [`respond`]
//! and gives it a [`Source`] that asks the server's own routes with the caller's
//! credentials, and sends every header of [`Answer::headers`].

use std::future::Future;

mod api;
mod html;
mod layout;
mod pages;
mod read;

/// The path every page is under.
pub const PREFIX: &str = "/ui";

/// The `Content-Security-Policy` of every answer: nothing loads but this server's
/// stylesheet (and, should one be added, images and one script from it); no inline
/// script or style runs; no other site may frame a page.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'self'; img-src 'self'; \
     script-src 'self'; frame-ancestors 'none'";

/// The stylesheet's path.
const STYLESHEET: &str = "/ui/static/kbf.css";

/// The stylesheet, embedded.
const CSS: &str = include_str!("../static/kbf.css");

/// An answer from the read API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    /// The HTTP status.
    pub status: u16,
    /// The body.
    pub body: Vec<u8>,
    /// The answer's headers, any or all of them. A page that answers with the API's
    /// own status passes on the ones that status depends on, `WWW-Authenticate` and
    /// `Retry-After` (RFC 9110 11.6.1 and 10.2.3), and no others.
    pub headers: Vec<(String, String)>,
}

/// The read API could not be asked.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SourceError(pub String);

/// Where the pages read the API: the server's own `/v1` routes, asked with the
/// caller's credentials, so a page shows only what its caller may read.
pub trait Source {
    /// `GET url`, a root-relative path with its query.
    fn get(&self, url: &str) -> impl Future<Output = Result<Fetched, SourceError>> + Send;
}

/// A page's answer: status, media type and body; [`Answer::headers`] adds the
/// security headers every answer carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    /// The read API's headers a page with its status passes on.
    passed_on: Vec<(&'static str, String)>,
}

impl Answer {
    const HTML: &'static str = "text/html; charset=utf-8";

    fn html(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: Self::HTML,
            body: body.into_bytes(),
            passed_on: Vec::new(),
        }
    }

    /// This answer with `headers` of the read API's answer added.
    fn passing_on(mut self, headers: &[(&'static str, String)]) -> Self {
        self.passed_on.extend_from_slice(headers);
        self
    }

    /// The HTTP status.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Every header of the answer: `Content-Type`, `Content-Security-Policy`,
    /// `X-Content-Type-Options`, and, on a page that answers with the read API's own
    /// status, the API's `WWW-Authenticate` and `Retry-After` when it sent them.
    #[must_use]
    pub fn headers(&self) -> Vec<(&'static str, &str)> {
        let fixed = [
            ("content-type", self.content_type),
            ("content-security-policy", CONTENT_SECURITY_POLICY),
            ("x-content-type-options", "nosniff"),
        ];
        let passed_on = self.passed_on.iter().map(|(k, v)| (*k, v.as_str()));
        fixed.into_iter().chain(passed_on).collect()
    }

    /// The body.
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// The answer to `GET path?query` under [`PREFIX`]: a page, the stylesheet, or a
/// `404` page (for a path that names no page, one with a segment that is not UTF-8,
/// or one with a segment that decodes to `.` or `..`). `path` is as sent
/// (percent-encoded); `query` is without its `?`.
pub async fn respond<S: Source + Sync>(source: &S, path: &str, query: &str) -> Answer {
    if path == STYLESHEET {
        return Answer {
            status: 200,
            content_type: "text/css; charset=utf-8",
            body: CSS.as_bytes().to_vec(),
            passed_on: Vec::new(),
        };
    }
    let rest = match path.strip_prefix(PREFIX) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => return pages::not_found(path),
    };
    // Each segment after the prefix, decoded; one that is not UTF-8 names no page.
    let decoded: Option<Vec<String>> = rest
        .split('/')
        .skip(1)
        .map(|s| {
            let s = percent_encoding::percent_decode_str(s).decode_utf8().ok()?;
            Some(s.into_owned())
        })
        .collect();
    // A segment that decodes to `.` or `..` names no page: written into a read API
    // URL it would be resolved away, so the page would read another route.
    let Some(segments) = decoded.filter(|s| !s.iter().any(|s| s == "." || s == "..")) else {
        return pages::not_found(path);
    };
    match segments.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] | [""] => pages::builds(source, query).await,
        ["builds", id] if !id.is_empty() => pages::build(source, id, query).await,
        ["builds", id, "actions", digest] if !id.is_empty() && !digest.is_empty() => {
            pages::action(source, id, digest).await
        }
        _ => pages::not_found(path),
    }
}

#[cfg(test)]
mod tests;
