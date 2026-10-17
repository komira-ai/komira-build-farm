//! The pages, each a [`Page`] (its routes) and an async function that reads them
//! through a [`Reader`] and writes the page.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::api::{Attempt, Counts, Invocation, Outcome, Text};
use crate::html::{Href, Html, attr};
use crate::layout;
use crate::read::{Page, ReadError, Reader};
use crate::{Answer, PREFIX, Source};

mod action;
mod build;

pub(crate) use action::action;
#[cfg(test)]
pub(crate) use action::{ACTION, LOG_SHOWN_MAX_BYTES};
#[cfg(test)]
pub(crate) use build::BUILD;
pub(crate) use build::build;

/// Builds: look a build up by id.
pub(crate) const BUILDS: Page = Page {
    name: "Builds",
    reads: &["/v1/invocations/{id}"],
};

/// Every page, for the contract test.
#[cfg(test)]
pub(crate) const PAGES: [&Page; 3] = [&BUILDS, &BUILD, &ACTION];

/// What a path segment keeps unencoded: RFC 3986's unreserved characters.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// `s` as one path segment.
pub(crate) fn segment(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT).to_string()
}

/// `/v1/invocations/{id}`.
pub(crate) fn invocation_url(id: &str) -> String {
    format!("/v1/invocations/{}", segment(id))
}

/// `/ui/builds/{id}`.
pub(crate) fn build_page(id: &str) -> String {
    format!("{PREFIX}/builds/{}", segment(id))
}

/// The first value of `key` in `query`.
pub(crate) fn query_value(query: &str, key: &str) -> Option<String> {
    form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// The answer for a page whose reads failed, with [`ReadError::status`].
pub(crate) fn failed(
    page: &Page,
    here: &str,
    asked: &[crate::read::Asked],
    e: &ReadError,
) -> Answer {
    let body = layout::page(page.name, here, asked, |h| {
        h.el("h1", &[], |h| {
            h.text(page.name);
        });
        problem(h, e);
    });
    Answer::html(e.status(), body).passing_on(e.headers())
}

/// A read's failure as a note on the page.
pub(crate) fn problem(h: &mut Html, e: &ReadError) {
    let class = if e.is_page_bug() {
        "problem bug"
    } else {
        "problem"
    };
    h.el("p", &[attr("class", class)], |h| {
        h.text(&e.to_string());
    });
}

/// The `404` page for a path that names no page.
pub(crate) fn not_found(path: &str) -> Answer {
    let body = layout::page("Not found", path, &[], |h| {
        h.el("h1", &[], |h| {
            h.text("Not found");
        });
        h.el("p", &[], |h| {
            h.text("No page is at ")
                .el("code", &[], |h| {
                    h.text(path);
                })
                .text(".");
        });
    });
    Answer::html(404, body)
}

/// The Builds page: a form for a build's id and, with `?id=`, that build's summary.
pub(crate) async fn builds<S: Source>(source: &S, query: &str) -> Answer {
    let id = query_value(query, "id").filter(|id| !id.is_empty());
    // An id of `.` or `..` names no build: written into `/v1/invocations/{id}` it is a
    // dot segment, which a URL client resolves to another route.
    let dot = matches!(id.as_deref(), Some("." | ".."));
    let mut r = Reader::new(source, &BUILDS);
    let found = match &id {
        Some(id) if !dot => Some(r.json::<Invocation>(&invocation_url(id)).await),
        _ => None,
    };
    let (status, passed_on) = match &found {
        Some(Err(e)) => (e.status(), e.headers()),
        _ if dot => (404, &[][..]),
        _ => (200, &[][..]),
    };
    let body = layout::page(BUILDS.name, PREFIX, r.asked(), |h| {
        h.el("h1", &[], |h| {
            h.text("Builds");
        });
        lookup(h, id.as_deref().unwrap_or_default());
        match &found {
            None if dot => {
                h.el("p", &[attr("class", "problem")], |h| {
                    h.text("A build id of . or .. names no build.");
                });
            }
            None => {
                h.el("p", &[attr("class", "hint")], |h| {
                    h.text(
                        "A list of recent builds comes with the history routes; until then, \
                         look a build up by its id (buck2's build id, or the id your client sends).",
                    );
                });
            }
            Some(Ok(inv)) => {
                h.el("section", &[attr("class", "card")], |h| {
                    h.el("h2", &[], |h| {
                        let href = Href::root_relative(&build_page(&inv.id))
                            .expect("a build page's path is root-relative");
                        h.link(&href, &inv.id);
                    });
                    summary(h, inv);
                });
            }
            Some(Err(e)) => problem(h, e),
        }
    });
    Answer::html(status, body).passing_on(passed_on)
}

/// The id form; `id` fills it in.
fn lookup(h: &mut Html, id: &str) {
    let action = Href::root_relative(PREFIX).expect("the prefix is root-relative");
    h.el(
        "form",
        &[
            attr("method", "get"),
            crate::html::url("action", &action),
            attr("class", "lookup"),
        ],
        |h| {
            h.el("label", &[attr("for", "id")], |h| {
                h.text("Build id");
            });
            h.void(
                "input",
                &[
                    attr("id", "id"),
                    attr("name", "id"),
                    attr("type", "text"),
                    attr("value", id),
                    attr("required", "required"),
                    attr("autocomplete", "off"),
                    attr("spellcheck", "false"),
                ],
            );
            h.void("input", &[attr("type", "submit"), attr("value", "Look up")]);
        },
    );
}

/// The invocation's header fields and counters, as a definition list.
pub(crate) fn summary(h: &mut Html, inv: &Invocation) {
    let tool = match (&inv.tool, &inv.tool_version) {
        (Some(t), Some(v)) => format!("{t} {v}"),
        (Some(t), None) => t.clone(),
        (None, _) => "unknown".to_owned(),
    };
    let state = format!(
        "{}{}{}",
        inv.state,
        if inv.complete { ", complete" } else { "" },
        if inv.rows_capped { ", rows capped" } else { "" },
    );
    let fields: [(&str, String); 6] = [
        ("State", state),
        ("Tool", tool),
        ("Instances", inv.instances.join(", ")),
        ("First seen", inv.first_seen.clone()),
        ("Last seen", inv.last_seen.clone()),
        ("Actions", inv.actions.to_string()),
    ];
    definitions(h, &fields);
    counts(h, &inv.counts);
}

fn counts(h: &mut Html, c: &Counts) {
    let cells: [(&str, String); 9] = [
        ("ran", c.ran.to_string()),
        ("failed", c.failed.to_string()),
        ("joined", c.joined.to_string()),
        ("execute", c.execute.to_string()),
        ("cache hit execute", c.cache_hit_execute.to_string()),
        ("AC hit", c.ac_hit.to_string()),
        ("AC miss", c.ac_miss.to_string()),
        ("uploaded", bytes(c.bytes_uploaded)),
        ("downloaded", bytes(c.bytes_downloaded)),
    ];
    h.el("ul", &[attr("class", "counts")], |h| {
        for (label, value) in &cells {
            h.el("li", &[], |h| {
                h.el("span", &[attr("class", "n")], |h| {
                    h.text(value);
                })
                .el("span", &[attr("class", "label")], |h| {
                    h.text(label);
                });
            });
        }
    });
}

/// A definition list of `fields`.
pub(crate) fn definitions(h: &mut Html, fields: &[(&str, String)]) {
    h.el("dl", &[attr("class", "fields")], |h| {
        for (name, value) in fields {
            h.el("dt", &[], |h| {
                h.text(name);
            })
            .el("dd", &[], |h| {
                h.text(value);
            });
        }
    });
}

/// `n` bytes in binary units, to one decimal past KiB.
pub(crate) fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// `us` microseconds as milliseconds or seconds.
pub(crate) fn micros(us: u64) -> String {
    if us < 1_000_000 {
        let ms = us as f64 / 1000.0;
        return format!("{ms:.1} ms");
    }
    let s = us as f64 / 1_000_000.0;
    format!("{s:.2} s")
}

/// What an action is called on a page: its target, else its mnemonic, else its
/// digest.
pub(crate) fn action_label(target: Option<&Text>, mnemonic: Option<&Text>, digest: &str) -> String {
    target.or(mnemonic).map_or_else(
        || digest.to_owned(),
        |t| {
            if t.truncated {
                format!("{}...", t.value)
            } else {
                t.value.clone()
            }
        },
    )
}

/// An outcome's words and the class that colours it.
pub(crate) fn outcome(o: Option<&Outcome>) -> (String, &'static str) {
    match o {
        None => ("pending".to_owned(), "pending"),
        Some(Outcome::Ok { exit_code: 0 }) => ("ok".to_owned(), "ok"),
        Some(Outcome::Ok { exit_code }) => (format!("exit {exit_code}"), "fail"),
        Some(Outcome::Failed { code, message }) => {
            (format!("failed (code {code}): {message}"), "fail")
        }
        Some(Outcome::TimedOut {}) => ("timed out".to_owned(), "fail"),
        Some(Outcome::Refused { reason }) => (format!("refused: {reason}"), "fail"),
        Some(Outcome::Other(tag)) => (tag.clone(), "other"),
    }
}

/// How an attempt ended, in words.
pub(crate) fn attempt_end(a: &Attempt) -> String {
    let Some(end) = &a.end else {
        return "running".to_owned();
    };
    let what = match (&end.result, &end.requeue, &end.failure) {
        (Some(r), _, _) => r.clone(),
        (None, Some(q), _) => format!("requeued: {q}"),
        (None, None, Some(f)) => format!("failed: {f}"),
        (None, None, None) => "ended".to_owned(),
    };
    match &end.reason {
        Some(reason) => format!("{what} ({reason})"),
        None => what,
    }
}

/// A status word in its coloured pill.
pub(crate) fn pill(h: &mut Html, words: &str, class: &'static str) {
    let class = format!("pill {class}");
    h.el("span", &[attr("class", &class)], |h| {
        h.text(words);
    });
}

#[cfg(test)]
mod tests;
